#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn unavailable_wsl_host_still_allows_a_safe_dry_run() {
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
    if !release.to_ascii_lowercase().contains("wsl2") {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let config_root = temporary.path().join("config");
    let state_root = temporary.path().join("state");
    let runtime = temporary.path().join("runtime");
    let scan_root = temporary.path().join("scan");
    for path in [&home, &config_root, &state_root, &runtime, &scan_root] {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&scan_root)
            .status()
            .unwrap()
            .success()
    );
    fs::write(scan_root.join(".gitignore"), b"/.turbo/\n").unwrap();
    fs::write(scan_root.join("package.json"), b"{\"private\":true}\n").unwrap();
    let cache = scan_root.join(".turbo");
    fs::create_dir_all(&cache).unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
    let marker = cache.join("regenerable");
    fs::write(&marker, b"synthetic").unwrap();
    let config_dir = config_root.join("guardwsl");
    fs::create_dir(&config_dir).unwrap();
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let config = config_dir.join("config.toml");
    let root = serde_json::to_string(&scan_root.to_string_lossy()).unwrap();
    fs::write(
        &config,
        format!("schema_version = 1\n[disk]\nhost_probe_timeout_seconds = 1\n[cleanup]\nscan_roots = [{root}]\nprotected_paths = []\n"),
    )
    .unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_guard"))
        .args(["clean", "--dry-run", "--all-caches", "--json"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_root)
        .env("XDG_STATE_HOME", &state_root)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("WSL_DISTRO_NAME", "guard-test-nonexistent-distribution")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid result: {error}; stderr={}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
    assert!(result["host_telemetry_error"].as_str().is_some());
    assert!(result["host_free_before_bytes"].is_null());
    assert!(result["host_free_after_bytes"].is_null());
    assert!(result["host_free_delta_bytes"].is_null());
    assert!(result["host_reclaim_possible"].is_null());
    assert_eq!(result["cleanup"]["deleted_logical_bytes"], 0);
    assert!(
        result["cleanup"]["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| {
                action["path"].as_str() == cache.to_str() && action["outcome"] == "would_remove"
            })
    );
    assert!(marker.exists());
}
