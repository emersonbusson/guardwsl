use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn guard(args: &[&str], state_home: Option<&Path>) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_guard"));
    command.args(args);
    if let Some(state_home) = state_home {
        command.env("XDG_STATE_HOME", state_home);
    }
    command.output().expect("guard command should start")
}

fn state_home_with_install_record(commit: Option<&str>) -> TempDir {
    let state_home = tempfile::tempdir().unwrap();
    let state_dir = state_home.path().join("guardwsl");
    fs::create_dir(&state_dir).unwrap();
    fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let mut record = serde_json::json!({
        "installed_at": "2026-09-27T12:00:00Z",
        "backup": "/tmp/previous-install",
        "version": env!("CARGO_PKG_VERSION")
    });
    if let Some(commit) = commit {
        record["commit"] = serde_json::json!(commit);
        record["commit_short"] = serde_json::json!(&commit[..12]);
    }
    let manifest = state_dir.join("install.json");
    fs::write(&manifest, serde_json::to_vec(&record).unwrap()).unwrap();
    fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600)).unwrap();
    state_home
}

#[test]
fn version_output_includes_the_source_commit() {
    let source_commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8(output.stdout).unwrap().trim().to_owned());

    for args in [&["--version"][..], &["-V"][..]] {
        let output = guard(args, None);
        assert!(output.status.success());
        let version = String::from_utf8(output.stdout).unwrap();
        let commit = version
            .trim()
            .split_once(" (commit ")
            .and_then(|(_, value)| value.strip_suffix(')'))
            .expect("version output should include a commit SHA");
        if let Some(source_commit) = &source_commit {
            assert_eq!(commit, source_commit);
        } else {
            assert_eq!(commit, "unknown");
        }
        assert!(
            commit == "unknown" || commit.len() >= 40,
            "version output should include the complete Git commit SHA when available"
        );
        assert!(commit == "unknown" || commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}

#[test]
fn status_json_includes_binary_identity_and_install_date() {
    let state_home =
        state_home_with_install_record(Some("0123456789abcdef0123456789abcdef01234567"));
    let output = guard(&["status", "--json"], Some(state_home.path()));
    let report: Value = serde_json::from_slice(&output.stdout)
        .expect("guard status --json should emit a JSON report");
    assert!(
        report["installed_at"] == "2026-09-27T12:00:00Z",
        "status JSON should include the installation timestamp"
    );

    let version_output = guard(&["--version"], None);
    let version_output = String::from_utf8(version_output.stdout).unwrap();
    let commit = version_output
        .trim()
        .split_once(" (commit ")
        .and_then(|(_, value)| value.strip_suffix(')'))
        .expect("--version should include a commit SHA");

    assert_eq!(report["commit"], commit);
    assert_eq!(report["commit_short"], &commit[..12]);
    assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(report["installed_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        report["installed_commit"],
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(report["installed_commit_short"], "0123456789ab");
}

#[test]
fn status_text_shows_version_commit_and_install_date() {
    let state_home =
        state_home_with_install_record(Some("0123456789abcdef0123456789abcdef01234567"));
    let output = guard(&["status"], Some(state_home.path()));
    let report = String::from_utf8(output.stdout).unwrap();

    let status_json = guard(&["status", "--json"], Some(state_home.path()));
    let status_json: Value = serde_json::from_slice(&status_json.stdout).unwrap();
    assert!(report.contains(&format!(
        "Version: {} (commit {})",
        env!("CARGO_PKG_VERSION"),
        status_json["commit_short"].as_str().unwrap()
    )));
    assert!(report.contains("Installed: 2026-09-27T12:00:00Z"));
}

#[test]
fn status_reads_install_records_without_a_commit_field() {
    let state_home = state_home_with_install_record(None);
    let output = guard(&["status", "--json"], Some(state_home.path()));
    let report: Value = serde_json::from_slice(&output.stdout)
        .expect("guard status --json should emit a JSON report");

    assert_eq!(report["installed_at"], "2026-09-27T12:00:00Z");
    assert_eq!(report["installed_version"], env!("CARGO_PKG_VERSION"));
    assert!(report["installed_commit"].is_null());
    assert!(report["installed_commit_short"].is_null());
}
