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
      "volume_root":"X:\\",
      "volume_total_bytes":1000,
      "volume_free_bytes":500
    }"#;

    let snapshot: HostSnapshot = serde_json::from_str(raw).unwrap();
    snapshot.validate().unwrap();
    assert_eq!(snapshot.volume_free_bytes, 500);
    let reported = serde_json::to_value(&snapshot).unwrap();
    assert!(reported.get("host_total_memory_bytes").is_none());
    assert!(reported.get("host_available_memory_bytes").is_none());
}
