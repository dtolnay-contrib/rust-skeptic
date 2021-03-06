extern crate cargo_metadata;
extern crate walkdir;

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::time::SystemTime;

use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::{self, env};
use tempfile;

use self::walkdir::WalkDir;

error_chain! {
    errors { Fingerprint }
    foreign_links {
        Io(std::io::Error);
        Metadata(cargo_metadata::Error);
    }
}

#[derive(Clone, Copy)]
enum CompileType {
    Full,
    Check,
}

// An iterator over the root dependencies in a lockfile
#[derive(Debug)]
struct LockedDeps {
    dependencies: Vec<String>,
}

fn get_cargo_meta<P: AsRef<Path> + std::convert::AsRef<std::ffi::OsStr>>(
    pth: P,
) -> Result<cargo_metadata::Metadata> {
    Ok(cargo_metadata::MetadataCommand::new()
        .manifest_path(&pth)
        .exec()?)
}

impl LockedDeps {
    fn from_path<P: AsRef<Path>>(pth: P) -> Result<LockedDeps> {
        let pth = pth.as_ref().join("Cargo.toml");
        let metadata = get_cargo_meta(&pth)?;
        let workspace_members = metadata.workspace_members;
        let deps = metadata
            .resolve
            .ok_or("Missing dependency metadata")?
            .nodes
            .into_iter()
            .filter(|node| workspace_members.contains(&node.id))
            .flat_map(|node| node.dependencies.into_iter())
            .chain(workspace_members.clone());

        Ok(LockedDeps {
            dependencies: deps.map(|node| node.repr).collect(),
        })
    }
}

impl Iterator for LockedDeps {
    type Item = (String, String);

    fn next(&mut self) -> Option<(String, String)> {
        self.dependencies.pop().and_then(|val| {
            let mut it = val.split_whitespace();

            match (it.next(), it.next()) {
                (Some(name), Some(val)) => Some((name.replace("-", "_"), val.to_owned())),
                _ => None,
            }
        })
    }
}

#[derive(Debug)]
struct Fingerprint {
    libname: String,
    version: Option<String>, // version might not be present on path or vcs deps
    rlib: PathBuf,
    mtime: SystemTime,
}

fn guess_ext(mut pth: PathBuf, exts: &[&str]) -> Result<PathBuf> {
    for ext in exts {
        pth.set_extension(ext);
        if pth.exists() {
            return Ok(pth);
        }
    }
    Err(ErrorKind::Fingerprint.into())
}

impl Fingerprint {
    fn from_path<P: AsRef<Path>>(pth: P) -> Result<Fingerprint> {
        let pth = pth.as_ref();

        // Use the parent path to get libname and hash, replacing - with _
        let mut captures = pth
            .parent()
            .and_then(Path::file_stem)
            .and_then(OsStr::to_str)
            .ok_or(ErrorKind::Fingerprint)?
            .rsplit('-');
        let hash = captures.next().ok_or(ErrorKind::Fingerprint)?;
        let mut libname_parts = captures.collect::<Vec<_>>();
        libname_parts.reverse();
        let libname = libname_parts.join("_");

        pth.extension()
            .and_then(|e| if e == "json" { Some(e) } else { None })
            .ok_or(ErrorKind::Fingerprint)?;

        let mut rlib = PathBuf::from(pth);
        rlib.pop();
        rlib.pop();
        rlib.pop();
        rlib.push(format!("deps/lib{}-{}", libname, hash));
        rlib = guess_ext(rlib, &["rlib", "so", "dylib", "dll"])?;

        let file = File::open(pth)?;
        let mtime = file.metadata()?.modified()?;

        Ok(Fingerprint {
            libname,
            version: None,
            rlib,
            mtime,
        })
    }

    fn name(&self) -> String {
        self.libname.clone()
    }

    fn version(&self) -> Option<String> {
        self.version.clone()
    }
}

fn get_edition<P: AsRef<Path>>(path: P) -> Result<String> {
    let path = path.as_ref().join("Cargo.toml");
    let metadata = get_cargo_meta(&path)?;
    let edition = metadata
        .packages
        .iter()
        .map(|package| &package.edition)
        .max_by_key(|edition| u64::from_str(edition).unwrap())
        .unwrap()
        .clone();
    Ok(edition)
}

// Retrieve the exact dependencies for a given build by
// cross-referencing the lockfile with the fingerprint file
fn get_rlib_dependencies<P: AsRef<Path>>(root_dir: P, target_dir: P) -> Result<Vec<Fingerprint>> {
    let root_dir = root_dir.as_ref();
    let target_dir = target_dir.as_ref();
    let lock = LockedDeps::from_path(root_dir).or_else(|_| {
        // could not find Cargo.lock in $CARGO_MAINFEST_DIR
        // try relative to target_dir
        let mut root_dir = PathBuf::from(target_dir);
        root_dir.pop();
        root_dir.pop();
        LockedDeps::from_path(root_dir)
    })?;

    let fingerprint_dir = target_dir.join(".fingerprint/");
    let locked_deps: HashMap<String, String> = lock.collect();
    let mut found_deps: HashMap<String, Fingerprint> = HashMap::new();

    for finger in WalkDir::new(fingerprint_dir)
        .into_iter()
        .filter_map(|v| v.ok())
        .filter_map(|v| Fingerprint::from_path(v.path()).ok())
    {
        if let Some(locked_ver) = locked_deps.get(&finger.name()) {
            // TODO this should be refactored to something more readable
            match (found_deps.entry(finger.name()), finger.version()) {
                (Entry::Occupied(mut e), Some(ver)) => {
                    // we find better match only if it is exact version match
                    // and has fresher build time
                    if *locked_ver == ver && e.get().mtime < finger.mtime {
                        e.insert(finger);
                    }
                }
                (Entry::Vacant(e), ver) => {
                    // we see an exact match or unversioned version
                    if ver.unwrap_or_else(|| locked_ver.clone()) == *locked_ver {
                        e.insert(finger);
                    }
                }
                _ => (),
            }
        }
    }

    Ok(found_deps
        .into_iter()
        .filter_map(|(_, val)| if val.rlib.exists() { Some(val) } else { None })
        .collect())
}

fn temp_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new().prefix(prefix).tempdir().unwrap()
}

pub fn compile_test(root_dir: &str, out_dir: &str, target_triple: &str, test_text: &str) {
    let rustc = &env::var("RUSTC").unwrap_or_else(|_| String::from("rustc"));
    let outdir = &temp_dir("rust-skeptic");
    let testcase_path = &outdir.path().join("test.rs");
    let binary_path = &outdir.path().join("out.exe");

    write_test_case(testcase_path, test_text);
    compile_test_case(
        testcase_path,
        binary_path,
        rustc,
        root_dir,
        out_dir,
        target_triple,
        CompileType::Check,
    );
}

pub fn run_test(root_dir: &str, out_dir: &str, target_triple: &str, test_text: &str) {
    let rustc = &env::var("RUSTC").unwrap_or_else(|_| String::from("rustc"));
    let outdir = &temp_dir("rust-skeptic");
    let testcase_path = &outdir.path().join("test.rs");
    let binary_path = &outdir.path().join("out.exe");

    write_test_case(testcase_path, test_text);
    compile_test_case(
        testcase_path,
        binary_path,
        rustc,
        root_dir,
        out_dir,
        target_triple,
        CompileType::Full,
    );
    run_test_case(binary_path, outdir.path());
}

fn write_test_case(path: &Path, test_text: &str) {
    let mut file = File::create(path).unwrap();
    file.write_all(test_text.as_bytes()).unwrap();
}

fn compile_test_case(
    in_path: &Path,
    out_path: &Path,
    rustc: &str,
    root_dir: &str,
    out_dir: &str,
    target_triple: &str,
    compile_type: CompileType,
) {
    // OK, here's where a bunch of magic happens using assumptions
    // about cargo internals. We are going to use rustc to compile
    // the examples, but to do that we've got to tell it where to
    // look for the rlibs with the -L flag, and what their names
    // are with the --extern flag. This is going to involve
    // parsing fingerprints out of the lockfile and looking them
    // up in the fingerprint file.

    let root_dir = PathBuf::from(root_dir);
    let mut target_dir = PathBuf::from(out_dir);
    target_dir.pop();
    target_dir.pop();
    target_dir.pop();
    let mut deps_dir = target_dir.clone();
    deps_dir.push("deps");

    let mut cmd = Command::new(rustc);
    cmd.arg(in_path).arg("--verbose").arg("--crate-type=bin");

    // This has to come before "-L".
    let edition = get_edition(&root_dir).expect("failed to read Cargo.toml");
    if edition != "2015" {
        cmd.arg(format!("--edition={}", edition));
    }

    cmd.arg("-L")
        .arg(&target_dir)
        .arg("-L")
        .arg(&deps_dir)
        .arg("--target")
        .arg(&target_triple);

    for dep in get_rlib_dependencies(root_dir, target_dir).expect("failed to read dependencies") {
        cmd.arg("--extern");
        cmd.arg(format!(
            "{}={}",
            dep.libname,
            dep.rlib.to_str().expect("filename not utf8"),
        ));
    }

    match compile_type {
        CompileType::Full => cmd.arg("-o").arg(out_path),
        CompileType::Check => cmd.arg(format!(
            "--emit=dep-info={0}.d,metadata={0}.m",
            out_path.display()
        )),
    };

    interpret_output(cmd);
}

fn run_test_case(program_path: &Path, outdir: &Path) {
    let mut cmd = Command::new(program_path);
    cmd.current_dir(outdir);
    interpret_output(cmd);
}

fn interpret_output(mut command: Command) {
    let output = command.output().unwrap();
    print!("{}", String::from_utf8(output.stdout).unwrap());
    eprint!("{}", String::from_utf8(output.stderr).unwrap());
    if !output.status.success() {
        panic!("Command failed:\n{:?}", command);
    }
}
