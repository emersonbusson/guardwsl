use guardwsl::host::HostSnapshot;

#[test]
fn host_snapshot_accepts_disk_only_v2_without_ram_fields() {
    let raw = r#"{
      "schema_version":2,
      "platform":"wsl2",
      "captured_at":"2026-08-23T12:00:00Z",
      "distro":"Example-WSL",
      "vhdx_path":"X:\\WSL\\Example-WSL\\ext4.vhdx",
      "vhdx_sparse":false,
      "vhdx_size_bytes":123456789,
      "volume_root":"X:\\",
      "volume_total_bytes":1000,
      "volume_free_bytes":500
    }"#;

    let snapshot: HostSnapshot = serde_json::from_str(raw).unwrap();
    snapshot.validate().unwrap();
    let mut unavailable_allocation = snapshot.clone();
    unavailable_allocation.vhdx_size_bytes = None;
    unavailable_allocation.validate().unwrap();
    assert_eq!(unavailable_allocation.volume_free_bytes, 500);
    assert!(guardwsl::host::vhdx_occupancy_ratio(&unavailable_allocation).is_none());
    let from_windows: HostSnapshot = serde_json::from_str(
        &raw.replace("2026-08-23T12:00:00Z", "2026-08-23T09:00:00.0000000-03:00"),
    )
    .unwrap();
    assert_eq!(from_windows.captured_at, snapshot.captured_at);
    assert_eq!(snapshot.volume_free_bytes, 500);
    let reported = serde_json::to_value(&snapshot).unwrap();
    assert!(reported.get("host_total_memory_bytes").is_none());
    assert!(reported.get("host_available_memory_bytes").is_none());
}
