//! Stamp the git commit into the binary.
//!
//! "Is the machine running the build I just deployed?" has cost this project
//! more debugging time than any actual bug — a stale process holding the
//! port, a unit in /etc pinning an old path, apt declining a package whose
//! version never changed. `CARGO_PKG_VERSION` can't answer it (it says 0.1.0
//! forever), so the commit goes in at compile time and gets logged at
//! startup and shown in the UI.
//!
//! A stamp is only worth having if it cannot be stale, which is the entire
//! job of `watch_git` below.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rustc-env=SOUNDNET_BUILD={}", describe());
    watch_git();
}

/// Tell cargo which files must invalidate this build script.
///
/// The obvious choice, `.git/HEAD`, is not enough and fails in the least
/// obvious direction: committing on the branch you are already on does not
/// touch `.git/HEAD` at all — it rewrites `.git/refs/heads/<branch>`, which
/// HEAD merely points at. Watching only HEAD means the script reruns when you
/// *switch* branches and never when you commit, so the binary keeps claiming
/// whatever commit it was first compiled at. Watch the resolved ref as well.
///
/// Nothing here is fatal: outside a git checkout (a source tarball) there is
/// simply nothing to watch, and asking cargo to watch a path that does not
/// exist would force a rebuild on every single invocation.
fn watch_git() {
    let Some(git_dir) = find_git_dir() else {
        return;
    };

    let head = git_dir.join("HEAD");
    if !head.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head.display());

    // "ref: refs/heads/main" → also watch .git/refs/heads/main. A detached
    // HEAD holds a bare hash instead, and then HEAD itself is the whole
    // story.
    let Ok(contents) = std::fs::read_to_string(&head) else {
        return;
    };
    let Some(refname) = contents.trim().strip_prefix("ref: ") else {
        return;
    };
    let ref_path = git_dir.join(refname);
    if ref_path.exists() {
        println!("cargo:rerun-if-changed={}", ref_path.display());
    } else {
        // Packed refs: a branch that has never moved since clone lives in
        // .git/packed-refs rather than as a loose file.
        let packed = git_dir.join("packed-refs");
        if packed.exists() {
            println!("cargo:rerun-if-changed={}", packed.display());
        }
    }
}

/// Walk up from the crate directory looking for `.git`. It is two levels up
/// in this workspace, but hardcoding that breaks the moment the layout
/// changes — and the failure would be a silently frozen stamp, not an error.
fn find_git_dir() -> Option<PathBuf> {
    let mut dir: &Path = &PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?);
    loop {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        // A worktree or submodule has `.git` as a file containing
        // "gitdir: <path>". Not worth resolving — `git` itself still works
        // from here, so the stamp is correct; only the rerun trigger is
        // missed, and `cargo clean` remains the fallback.
        if candidate.is_file() {
            return None;
        }
        dir = dir.parent()?;
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
