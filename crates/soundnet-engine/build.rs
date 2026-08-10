//! Stamp the git commit into the binary.
//!
//! "Is the machine running the build I just deployed?" has cost this project
//! more debugging time than any actual bug — a stale process holding the
//! port, a browser serving a cached UI, an install that went to a different
//! prefix. `CARGO_PKG_VERSION` can't answer it (it says 0.1.0 forever), so
//! the commit goes in at compile time and gets logged at startup and shown
//! in the UI. An unanswerable question becomes a one-line check.

use std::process::Command;

fn main() {
    println!("cargo:rustc-env=SOUNDNET_BUILD={}", describe());
    // Rebuild when HEAD moves, so the stamp can't itself go stale. Absent in
    // a source tarball, and `cargo:rerun-if-changed` on a missing path would
    // force a rebuild every single time — so only ask for it if it's there.
    for path in [".git/HEAD", "../../.git/HEAD"] {
        if std::path::Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// `1e0ce0a` or `1e0ce0a-dirty`, falling back to "unknown" outside a git
/// checkout. Never fails the build: not knowing the commit is a much smaller
/// problem than not being able to compile without git installed.
fn describe() -> String {
    let Some(hash) = run(&["rev-parse", "--short=7", "HEAD"]) else {
        return "unknown".to_string();
    };
    // `--porcelain` prints one line per modified path, so any output at all
    // means the working tree doesn't match the commit we just named — and a
    // stamp that claims a commit it isn't really running would be worse than
    // no stamp.
    match run(&["status", "--porcelain", "--untracked-files=no"]) {
        Some(s) if !s.is_empty() => format!("{hash}-dirty"),
        _ => hash,
    }
}

fn run(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
