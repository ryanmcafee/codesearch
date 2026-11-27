use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/refs/heads");

    // Get the version from Cargo.toml
    let cargo_version = env::var("CARGO_PKG_VERSION").unwrap();

    // Get git commit count
    let commit_count = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "0".to_string());

    // Get current git commit hash (short)
    let commit_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Get current branch name
    let branch_name = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Construct full version string
    let version_full = format!("{}+{}", cargo_version, commit_count);

    // Set environment variables for the main binary. Emitted under the
    // CODESEARCH_ prefix so they are identifiable in build output as this
    // project's provenance (the DEMONGREP_ prefix was a leftover from the
    // project this fork started from; nothing read it).
    println!("cargo:rustc-env=CODESEARCH_VERSION_FULL={}", version_full);
    println!("cargo:rustc-env=CODESEARCH_COMMIT_HASH={}", commit_hash);
    println!("cargo:rustc-env=CODESEARCH_COMMIT_COUNT={}", commit_count);
    println!("cargo:rustc-env=CODESEARCH_BRANCH={}", branch_name);

    // Also set for display in --version output
    println!("cargo:rustc-env=CARGO_PKG_VERSION_FULL={}", version_full);

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        link_clang_runtime();
    }
}

// ONNX Runtime's prebuilt CoreML objects call `@available`, which needs `___isPlatformVersionAtLeast`.
fn link_clang_runtime() {
    let resource_dir = Command::new("xcrun")
        .args(["clang", "--print-resource-dir"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string());

    let Some(resource_dir) = resource_dir else {
        println!(
            "cargo:warning=`xcrun clang --print-resource-dir` failed; libclang_rt.osx.a not linked"
        );
        return;
    };

    let runtime = std::path::Path::new(&resource_dir).join("lib/darwin/libclang_rt.osx.a");
    if !runtime.exists() {
        println!(
            "cargo:warning={} not found; link may fail on undefined ___isPlatformVersionAtLeast",
            runtime.display()
        );
        return;
    }
    println!("cargo:rustc-link-arg={}", runtime.display());
}
