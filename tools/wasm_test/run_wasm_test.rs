//! Runs a Rust test built for the web under wasmtime, as a Bazel test.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

use runfiles::Runfiles;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    // std on wasm32-unknown-unknown reads no arguments, so libtest would run
    // every test whatever filter or flag it was given.
    if let Some(filter) = env::var_os("TESTBRIDGE_TEST_ONLY") {
        return Err(format!(
            "a test built for the web runs all of its tests; it cannot take the filter {}",
            filter.to_string_lossy()
        ));
    }
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if !args.is_empty() {
        return Err(format!(
            "a test built for the web runs all of its tests; it cannot take the arguments {args:?}"
        ));
    }

    let runfiles = Runfiles::create().map_err(|e| format!("cannot find the runfiles: {e}"))?;
    let wasmtime = runfile(&runfiles, "WASMTIME")?;
    let module = runfile(&runfiles, "WASM_TEST_MODULE")?;

    // The module exports the C entry point main(argc, argv), which wasmtime
    // runs only when invoked by name. std there discards all output and has
    // no process to exit, so libtest passes by returning 0 from main and fails
    // by trapping, naming no test: run the crate's native test to see which
    // failed. Imports from JavaScript, which wasmtime cannot provide, trap if a
    // test reaches them.
    let output = Command::new(&wasmtime)
        .args(["run", "-W", "unknown-imports-trap=y", "--invoke", "main"])
        .arg(&module)
        .args(["0", "0"])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", wasmtime.display()))?;
    if !output.status.success() {
        return Err(format!("{} failed: {}", module.display(), output.status));
    }
    let returned = String::from_utf8_lossy(&output.stdout);
    if returned.trim_end() != "0" {
        return Err(format!(
            "{} returned {:?} from main, not 0",
            module.display(),
            returned.trim_end()
        ));
    }
    Ok(())
}

/// The runfile at the runfiles path the environment variable `name` holds.
fn runfile(runfiles: &Runfiles, name: &str) -> Result<PathBuf, String> {
    let path = env::var(name).map_err(|e| format!("{name}: {e}"))?;
    runfiles
        .rlocation(&path)
        .ok_or_else(|| format!("{name}: no runfile {path}"))
}
