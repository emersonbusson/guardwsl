use guardwsl::config::{DiskConfig, GIB};
use guardwsl::host::{
    DiskPressure, HostPlatform, classify_disk, classify_platform, probe_linux_disk,
};
use std::os::unix::fs::symlink;

#[test]
fn platform_detection_keeps_wsl2_and_native_linux_distinct() {
    assert_eq!(
        classify_platform("6.18.40.1-microsoft-standard-WSL2+").unwrap(),
        HostPlatform::Wsl2
    );
    assert_eq!(
        classify_platform("6.12.0-42-generic").unwrap(),
        HostPlatform::Linux
    );
    assert!(classify_platform("4.4.0-Microsoft").is_err());
}

#[test]
fn native_probe_uses_local_filesystem_and_refuses_symlink_roots() {
    let directory = tempfile::tempdir().unwrap();
    let snapshot = probe_linux_disk(&[directory.path().to_path_buf()]).unwrap();
    assert_eq!(snapshot.platform, HostPlatform::Linux);
    assert!(snapshot.volume_total_bytes > 0);
    assert!(snapshot.volume_free_bytes <= snapshot.volume_total_bytes);
    assert!(snapshot.vhdx_path.is_none());

    let linked = directory.path().join("linked-root");
    symlink(directory.path(), &linked).unwrap();
    assert!(probe_linux_disk(&[linked]).is_err());
}

#[test]
fn tiny_disks_are_not_permanently_in_emergency() {
    let config = DiskConfig::default();
    let total = 5 * GIB;
    let thresholds = config.effective_thresholds(total);
    assert!(thresholds.target_free_bytes < total);
    assert_eq!(classify_disk(2 * GIB, total, &config), DiskPressure::Healthy);
    assert_eq!(classify_disk(200 * 1024 * 1024, total, &config), DiskPressure::Emergency);
}
