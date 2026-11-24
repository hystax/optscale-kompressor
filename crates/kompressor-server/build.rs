//! The commit this build is from, for the Maintenance page: what is running
//! is the first thing to establish when something looks wrong, and a version
//! number names many commits.
//!
//! `KOMPRESSOR_GIT_COMMIT` from the environment wins (a packaging build from
//! a tarball has no `.git`); otherwise `git rev-parse`. Neither is empty, and
//! the page says the build could not tell.
//!
//! And when it was built: `SOURCE_DATE_EPOCH` where a reproducible build sets
//! it, else the clock. This script reruns when `HEAD` moves, not on every
//! build, so the time is that of the first build of this commit in this
//! target directory — a rebuild after an uncommitted edit keeps it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=KOMPRESSOR_GIT_COMMIT");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let git = manifest.join("../../.git");
    watch(&git);
    let commit = std::env::var("KOMPRESSOR_GIT_COMMIT")
        .ok()
        .filter(|commit| !commit.trim().is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .current_dir(&manifest)
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .unwrap_or_default();
    println!("cargo:rustc-env=KOMPRESSOR_GIT_COMMIT={commit}");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let built = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|epoch| epoch.trim().parse::<u64>().ok())
        .or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|since| since.as_secs())
        })
        .unwrap_or(0);
    println!("cargo:rustc-env=KOMPRESSOR_BUILD_EPOCH={built}");
}

/// Rebuild when HEAD moves: `HEAD` itself, and the branch it names. Only
/// files that exist are named — a missing one would rerun this every build.
fn watch(git: &Path) {
    let head = git.join("HEAD");
    if !head.is_file() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head.display());
    if let Ok(text) = std::fs::read_to_string(&head)
        && let Some(reference) = text.trim().strip_prefix("ref: ")
    {
        for candidate in [git.join(reference), git.join("packed-refs")] {
            if candidate.is_file() {
                println!("cargo:rerun-if-changed={}", candidate.display());
            }
        }
    }
}
