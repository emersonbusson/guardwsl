use std::path::PathBuf;
use std::process::Command;

fn git_output(manifest_dir: &str, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();

    if let Some(git_dir) = git_output(&manifest_dir, &["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
    }
    if let Some(common_dir) = git_output(&manifest_dir, &["rev-parse", "--git-common-dir"]) {
        let common_dir = PathBuf::from(&common_dir);
        let common_dir = if common_dir.is_absolute() {
            common_dir
        } else {
            PathBuf::from(&manifest_dir).join(common_dir)
        };
        println!(
            "cargo:rerun-if-changed={}",
            common_dir.join("packed-refs").display()
        );
        if let Some(reference) = git_output(&manifest_dir, &["symbolic-ref", "--quiet", "HEAD"]) {
            println!(
                "cargo:rerun-if-changed={}",
                common_dir.join(reference).display()
            );
        }
    }

    let commit = git_output(&manifest_dir, &["rev-parse", "HEAD"])
        .filter(|value| value.len() >= 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".to_owned());
    let commit_short = if commit == "unknown" {
        "unknown".to_owned()
    } else {
        commit.chars().take(12).collect()
    };
    println!("cargo:rustc-env=GUARD_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=GUARD_GIT_COMMIT_SHORT={commit_short}");
}
