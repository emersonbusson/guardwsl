use serde_json::Value;
use std::process::Command;

fn guard(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_guard"))
        .args(args)
        .output()
        .expect("guard command should start")
}

#[test]
fn version_output_includes_the_source_commit() {
    let output = guard(&["--version"]);
    assert!(output.status.success());
    let version = String::from_utf8(output.stdout).unwrap();
    let commit = version
        .trim()
        .split_once(" (commit ")
        .and_then(|(_, value)| value.strip_suffix(')'))
        .expect("--version should include a commit SHA");
    assert_eq!(commit.len(), 12, "commit SHA should use 12 hex digits");
    assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
}

#[test]
fn status_json_includes_binary_identity_and_install_date() {
    let output = guard(&["status", "--json"]);
    let report: Value = serde_json::from_slice(&output.stdout)
        .expect("guard status --json should emit a JSON report");
    assert!(
        report["installed_at"].as_str().is_some(),
        "status JSON should include the installation timestamp"
    );

    let version_output = guard(&["--version"]);
    let version_output = String::from_utf8(version_output.stdout).unwrap();
    let commit = version_output
        .trim()
        .split_once(" (commit ")
        .and_then(|(_, value)| value.strip_suffix(')'))
        .expect("--version should include a commit SHA");

    assert_eq!(report["commit"], commit);
    assert!(report["version"].as_str().is_some());
}
