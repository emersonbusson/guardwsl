//! Single, compact configuration model for GuardWSL v1.

use crate::fsutil::{atomic_write_private, read_private};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const GIB: u64 = 1024 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuardConfig {
    pub schema_version: u32,
    pub admission: AdmissionConfig,
    pub disk: DiskConfig,
    pub memory: MemoryConfig,
    pub cleanup: CleanupConfig,
    pub monitor: MonitorConfig,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self::default_for_home(configured_home().as_deref())
    }
}

impl GuardConfig {
    fn default_for_home(home: Option<&Path>) -> Self {
        let scan_roots = home
            .filter(|path| path.is_absolute())
            .map(|path| vec![path.to_path_buf()])
            .unwrap_or_default();
        let protected_paths = home
            .filter(|path| path.is_absolute())
            .map(|path| {
                [
                    PathBuf::from("/etc"),
                    PathBuf::from("/var/lib/docker/volumes"),
                    path.join(".ssh"),
                    path.join(".gnupg"),
                    path.join(".config"),
                    path.join(".local/share/keyrings"),
                    path.join(".password-store"),
                    path.join(".aws"),
                    path.join(".azure"),
                    path.join(".kube"),
                ]
                .into_iter()
                .collect()
            })
            .unwrap_or_default();
        Self {
            schema_version: 1,
            admission: AdmissionConfig::default(),
            disk: DiskConfig::default(),
            memory: MemoryConfig::default(),
            cleanup: CleanupConfig {
                scan_roots,
                protected_paths,
                ..CleanupConfig::default()
            },
            monitor: MonitorConfig::default(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!("schema_version must be 1")
        }
        self.admission.validate()?;
        self.disk.validate()?;
        self.memory.validate()?;
        self.cleanup.validate()?;
        self.monitor.validate()?;
        Ok(())
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdmissionConfig {
    pub enabled: bool,
    pub build_wait_seconds: u64,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            build_wait_seconds: 4 * 60 * 60,
        }
    }
}

impl AdmissionConfig {
    fn validate(&self) -> Result<()> {
        if !(1..=24 * 60 * 60).contains(&self.build_wait_seconds) {
            bail!("admission.build_wait_seconds must be between 1 and 86400")
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiskConfig {
    pub pressure_free_bytes: u64,
    pub critical_free_bytes: u64,
    pub emergency_free_bytes: u64,
    pub target_free_bytes: u64,
    pub host_probe_timeout_seconds: u64,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            pressure_free_bytes: 48 * GIB,
            critical_free_bytes: 24 * GIB,
            emergency_free_bytes: 12 * GIB,
            target_free_bytes: 64 * GIB,
            host_probe_timeout_seconds: 10,
        }
    }
}

impl DiskConfig {
    fn validate(&self) -> Result<()> {
        if self.emergency_free_bytes == 0
            || self.emergency_free_bytes >= self.critical_free_bytes
            || self.critical_free_bytes >= self.pressure_free_bytes
            || self.pressure_free_bytes >= self.target_free_bytes
        {
            bail!(
                "thresholds must be positive and satisfy emergency < critical < pressure < target"
            )
        }
        if !(1..=30).contains(&self.host_probe_timeout_seconds) {
            bail!("disk.host_probe_timeout_seconds must be between 1 and 30")
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    pub host_floor_bytes: u64,
    pub build_headroom_bytes: u64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            host_floor_bytes: 8 * GIB,
            build_headroom_bytes: 4 * GIB,
        }
    }
}

impl MemoryConfig {
    fn validate(&self) -> Result<()> {
        if self.host_floor_bytes < 4 * GIB || self.build_headroom_bytes > 32 * GIB {
            bail!("memory floor/headroom is outside the safe bounds")
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanupConfig {
    pub enabled: bool,
    pub scan_roots: Vec<PathBuf>,
    pub protected_paths: Vec<PathBuf>,
    pub cache_min_age_hours: u64,
    pub build_min_age_hours: u64,
    pub node_modules_min_age_hours: u64,
    pub critical_min_age_hours: u64,
    pub max_actions_per_cycle: usize,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_roots: Vec::new(),
            protected_paths: Vec::new(),
            cache_min_age_hours: 7 * 24,
            build_min_age_hours: 7 * 24,
            node_modules_min_age_hours: 30 * 24,
            critical_min_age_hours: 24,
            max_actions_per_cycle: 20,
        }
    }
}

impl CleanupConfig {
    fn validate(&self) -> Result<()> {
        if self.scan_roots.is_empty() {
            bail!("cleanup.scan_roots cannot be empty")
        }
        if self.scan_roots.len() > 32 || self.protected_paths.len() > 256 {
            bail!("too many scan roots or protected paths")
        }
        for root in &self.scan_roots {
            if !root.is_absolute() {
                bail!("scan root must be absolute: {}", root.display())
            }
        }
        for path in &self.protected_paths {
            if !path.is_absolute() {
                bail!("protected path must be absolute: {}", path.display())
            }
        }
        if self.cache_min_age_hours == 0
            || self.build_min_age_hours == 0
            || self.node_modules_min_age_hours == 0
            || self.critical_min_age_hours == 0
            || !(1..=100).contains(&self.max_actions_per_cycle)
        {
            bail!("ages and max_actions_per_cycle must be positive and bounded")
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitorConfig {
    pub interval_seconds: u64,
    pub maintenance_interval_seconds: u64,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            interval_seconds: 30,
            maintenance_interval_seconds: 6 * 60 * 60,
        }
    }
}

impl MonitorConfig {
    fn validate(&self) -> Result<()> {
        if !(10..=300).contains(&self.interval_seconds) {
            bail!("monitor.interval_seconds must be between 10 and 300")
        }
        if !(15 * 60..=7 * 24 * 60 * 60).contains(&self.maintenance_interval_seconds) {
            bail!("monitor.maintenance_interval_seconds is outside the allowed bounds")
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigOrigin {
    Default,
    User,
    LastKnownGood,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: GuardConfig,
    pub origin: ConfigOrigin,
    pub degraded: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub user: PathBuf,
    pub last_known_good: PathBuf,
}

fn configured_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    paths: ConfigPaths,
}

impl ConfigStore {
    pub fn discover() -> Result<Self> {
        let root = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(value) => {
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    bail!("XDG_CONFIG_HOME must be absolute")
                }
                path
            }
            None => configured_home()
                .context("an absolute HOME or XDG_CONFIG_HOME is required")?
                .join(".config"),
        }
        .join("guardwsl");
        Ok(Self::new(ConfigPaths {
            user: root.join("config.toml"),
            last_known_good: root.join("config.last-good.toml"),
        }))
    }

    #[must_use]
    pub fn new(paths: ConfigPaths) -> Self {
        Self { paths }
    }

    #[must_use]
    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    pub fn load(&self) -> Result<LoadedConfig> {
        self.load_inner(true)
    }

    pub fn load_read_only(&self) -> Result<LoadedConfig> {
        self.load_inner(false)
    }

    fn load_inner(&self, refresh_lkg: bool) -> Result<LoadedConfig> {
        if !self.paths.user.exists() {
            if self.paths.last_known_good.exists() {
                let config = read_config(&self.paths.last_known_good)
                    .context("active configuration is missing and last-known-good is invalid")?;
                return Ok(LoadedConfig {
                    config,
                    origin: ConfigOrigin::LastKnownGood,
                    degraded: true,
                    warnings: vec!["active configuration is missing".to_owned()],
                });
            }
            let config = GuardConfig::default();
            config.validate()?;
            return Ok(LoadedConfig {
                config,
                origin: ConfigOrigin::Default,
                degraded: false,
                warnings: Vec::new(),
            });
        }
        match read_config(&self.paths.user) {
            Ok(config) => {
                if refresh_lkg {
                    atomic_write_private(
                        &self.paths.last_known_good,
                        config.to_toml()?.as_bytes(),
                    )?;
                }
                Ok(LoadedConfig {
                    config,
                    origin: ConfigOrigin::User,
                    degraded: false,
                    warnings: Vec::new(),
                })
            }
            Err(active_error) => {
                let fallback = read_config(&self.paths.last_known_good).with_context(|| {
                    format!(
                        "active configuration is invalid ({active_error:#}) and last-known-good is unavailable"
                    )
                })?;
                Ok(LoadedConfig {
                    config: fallback,
                    origin: ConfigOrigin::LastKnownGood,
                    degraded: true,
                    warnings: vec![active_error.to_string()],
                })
            }
        }
    }

    pub fn init_if_missing(&self) -> Result<bool> {
        if self.paths.user.exists() {
            return Ok(false);
        }
        self.save(&GuardConfig::default())?;
        Ok(true)
    }

    pub fn save(&self, config: &GuardConfig) -> Result<()> {
        config.validate()?;
        let text = config.to_toml()?;
        atomic_write_private(&self.paths.user, text.as_bytes())?;
        atomic_write_private(&self.paths.last_known_good, text.as_bytes())?;
        Ok(())
    }

    pub fn set_admission_enabled(&self, enabled: bool) -> Result<LoadedConfig> {
        let mut loaded = self.load_read_only()?;
        if loaded.degraded {
            bail!("configuration is degraded; fix it before changing admission")
        }
        loaded.config.admission.enabled = enabled;
        self.save(&loaded.config)?;
        self.load_read_only()
    }
}

const LEGACY_TABLES: &[&str] = &[
    "scan",
    "categories",
    "intervals",
    "reserve",
    "workloads",
    "host_memory",
    "archive",
];

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyConfig {
    version: Option<u32>,
    protected_paths: Vec<String>,
    workloads: LegacyWorkloads,
    host_memory: LegacyHostMemory,
    intervals: LegacyIntervals,
    categories: LegacyCategories,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyWorkloads {
    enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyHostMemory {
    host_floor_bytes: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyIntervals {
    monitor_seconds: Option<u64>,
    maintenance_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyCategories {
    #[serde(alias = "javascript")]
    javascript_caches: Option<LegacyCategory>,
    #[serde(alias = "go")]
    go_caches: Option<LegacyCategory>,
    #[serde(alias = "rust")]
    rust_caches: Option<LegacyCategory>,
    build_outputs: Option<LegacyCategory>,
    node_modules: Option<LegacyCategory>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyCategory {
    minimum_age_hours: Option<u64>,
}

fn is_legacy_document(document: &toml::Value) -> Result<bool> {
    let table = document
        .as_table()
        .context("the configuration root must be a TOML table")?;
    if table.contains_key("schema_version") && table.contains_key("version") {
        bail!("ambiguous configuration: schema_version and version coexist")
    }

    let has_legacy_table = LEGACY_TABLES.iter().any(|key| table.contains_key(*key));
    let Some(version) = table.get("version") else {
        return Ok(false);
    };
    if !has_legacy_table {
        return Ok(false);
    }

    let version = version
        .as_integer()
        .context("legacy version must be an integer")?;
    if version != 1 {
        bail!("unsupported legacy version: {version}")
    }
    Ok(true)
}

fn migrate_legacy_config(legacy: LegacyConfig, home: Option<&Path>) -> Result<GuardConfig> {
    let LegacyConfig {
        version,
        protected_paths,
        workloads,
        host_memory,
        intervals,
        categories,
    } = legacy;
    if version != Some(1) {
        bail!("migration requires version = 1")
    }

    let mut config = GuardConfig::default_for_home(home);
    if let Some(enabled) = workloads.enabled {
        config.admission.enabled = enabled;
    }
    if let Some(host_floor_bytes) = host_memory.host_floor_bytes {
        config.memory.host_floor_bytes = host_floor_bytes;
    }
    if let Some(interval_seconds) = intervals.monitor_seconds {
        config.monitor.interval_seconds = interval_seconds;
    }
    if let Some(maintenance_interval_seconds) = intervals.maintenance_seconds {
        config.monitor.maintenance_interval_seconds = maintenance_interval_seconds;
    }

    let cache_min_age_hours = [
        categories.javascript_caches.as_ref(),
        categories.go_caches.as_ref(),
        categories.rust_caches.as_ref(),
    ]
    .into_iter()
    .filter_map(|category| category.and_then(|value| value.minimum_age_hours))
    .max();
    if let Some(hours) = cache_min_age_hours {
        config.cleanup.cache_min_age_hours = hours;
    }
    if let Some(hours) = categories
        .build_outputs
        .and_then(|category| category.minimum_age_hours)
    {
        config.cleanup.build_min_age_hours = hours;
    }
    if let Some(hours) = categories
        .node_modules
        .and_then(|category| category.minimum_age_hours)
    {
        config.cleanup.node_modules_min_age_hours = hours;
    }

    let mut migrated_protected_paths = Vec::with_capacity(protected_paths.len());
    for raw in protected_paths {
        let path = migrate_legacy_protected_path(&raw, home)?;
        if !migrated_protected_paths.contains(&path) {
            migrated_protected_paths.push(path);
        }
    }
    config.cleanup.protected_paths = migrated_protected_paths;
    Ok(config)
}

fn migrate_legacy_protected_path(raw: &str, home: Option<&Path>) -> Result<PathBuf> {
    let path = if let Some(relative) = raw.strip_prefix("~/") {
        let home = home
            .filter(|path| path.is_absolute())
            .context("an absolute HOME is required to expand protected_paths starting with ~/")?;
        home.join(relative)
    } else {
        let path = PathBuf::from(raw);
        if raw.starts_with('~') || !path.is_absolute() {
            bail!("legacy protected path must be absolute or start with ~/: {raw}")
        }
        path
    };
    if !path.is_absolute() {
        bail!("legacy protected path did not produce an absolute path: {raw}")
    }
    Ok(path)
}

fn parse_config_text(text: &str, home: Option<&Path>) -> Result<GuardConfig> {
    let document: toml::Value =
        toml::from_str(text).context("configuration is not a valid TOML document")?;
    let config = if is_legacy_document(&document)? {
        let legacy: LegacyConfig = document
            .try_into()
            .context("legacy version=1 configuration is invalid")?;
        migrate_legacy_config(legacy, home)?
    } else {
        document
            .try_into()
            .context("schema_version=1 configuration is invalid")?
    };
    config.validate()?;
    Ok(config)
}

fn read_config(path: &Path) -> Result<GuardConfig> {
    let bytes = read_private(path, MAX_CONFIG_BYTES)?;
    let text = std::str::from_utf8(&bytes).context("configuration is not UTF-8")?;
    parse_config_text(text, configured_home().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_default() -> GuardConfig {
        GuardConfig::default_for_home(Some(Path::new("/home/guard-test")))
    }

    fn store() -> (tempfile::TempDir, ConfigStore) {
        let directory = tempdir().unwrap();
        let store = ConfigStore::new(ConfigPaths {
            user: directory.path().join("config.toml"),
            last_known_good: directory.path().join("config.last-good.toml"),
        });
        (directory, store)
    }

    #[test]
    fn defaults_keep_disk_safety_independent_from_admission() {
        let config = test_default();
        assert!(config.admission.enabled);
        assert!(config.cleanup.enabled);
        assert_eq!(
            config.cleanup.scan_roots,
            vec![PathBuf::from("/home/guard-test")]
        );
        assert!(
            config
                .cleanup
                .protected_paths
                .contains(&PathBuf::from("/home/guard-test/.ssh"))
        );
        assert_eq!(config.memory.host_floor_bytes, 8 * GIB);
        assert!(config.disk.emergency_free_bytes < config.disk.critical_free_bytes);
        assert!(config.disk.critical_free_bytes < config.disk.pressure_free_bytes);
        assert!(config.disk.pressure_free_bytes < config.disk.target_free_bytes);
    }

    #[test]
    fn generated_default_configuration_round_trips_strictly() {
        let expected = test_default();
        let text = expected.to_toml().unwrap();
        let parsed = parse_config_text(&text, Some(Path::new("/home/guard-test"))).unwrap();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn admission_toggle_persists_without_touching_cleanup() {
        let (_directory, store) = store();
        store.save(&test_default()).unwrap();
        let off = store.set_admission_enabled(false).unwrap();
        assert!(!off.config.admission.enabled);
        assert!(off.config.cleanup.enabled);
        let on = store.set_admission_enabled(true).unwrap();
        assert!(on.config.admission.enabled);
    }

    #[test]
    fn invalid_active_config_uses_last_known_good_and_blocks_mutation() {
        let (_directory, store) = store();
        store.save(&test_default()).unwrap();
        std::fs::write(&store.paths.user, b"not = [valid").unwrap();
        let loaded = store.load_read_only().unwrap();
        assert!(loaded.degraded);
        assert_eq!(loaded.origin, ConfigOrigin::LastKnownGood);
        assert!(store.set_admission_enabled(false).is_err());
    }

    #[test]
    fn disappearing_active_config_uses_last_known_good_in_degraded_mode() {
        let (_directory, store) = store();
        let expected = test_default();
        store.save(&expected).unwrap();
        std::fs::remove_file(&store.paths.user).unwrap();
        let loaded = store.load_read_only().unwrap();
        assert!(loaded.degraded);
        assert_eq!(loaded.origin, ConfigOrigin::LastKnownGood);
        assert_eq!(loaded.config, expected);
    }

    #[test]
    fn contradictory_thresholds_are_rejected() {
        let mut config = test_default();
        config.disk.critical_free_bytes = config.disk.pressure_free_bytes;
        assert!(config.validate().is_err());
    }

    #[test]
    fn missing_home_leaves_defaults_invalid_without_mutating_process_environment() {
        let config = GuardConfig::default_for_home(None);
        assert!(config.cleanup.scan_roots.is_empty());
        assert!(config.validate().is_err());

        let legacy = "version = 1\n[workloads]\nenabled = true\n";
        assert!(parse_config_text(legacy, None).is_err());
    }

    #[test]
    fn migrates_real_legacy_shape_deterministically_and_ignores_retired_subsystems() {
        let legacy = r#"
version = 1
protected_paths = ["/etc", "~/.ssh", "/etc"]

[scan]
roots = ["/home"]
max_depth = 12

[categories.javascript_caches]
enabled = true
minimum_age_hours = 24

[categories.go_caches]
minimum_age_hours = 36

[categories.rust_caches]
minimum_age_hours = 48

[categories.build_outputs]
minimum_age_hours = 72

[categories.node_modules]
minimum_age_hours = 720

[categories.docker_images]
enabled = true
minimum_age_hours = 1

[intervals]
monitor_seconds = 45
maintenance_seconds = 7200
pressure_cooldown_seconds = 1

[reserve]
hard_reserve_bytes = 17179869184

[workloads]
enabled = false
apply_managed_cgroup_controls = true

[workloads.policy]
max_concurrent = 99

[host_memory]
host_floor_bytes = 10737418240
require_hyper_v_inventory = true

[archive]
enabled = true
"#;

        let config = parse_config_text(legacy, Some(Path::new("/home/guard-test"))).unwrap();
        assert_eq!(config.schema_version, 1);
        assert!(!config.admission.enabled);
        assert_eq!(config.memory.host_floor_bytes, 10 * GIB);
        assert_eq!(config.monitor.interval_seconds, 45);
        assert_eq!(config.monitor.maintenance_interval_seconds, 7200);
        assert_eq!(config.cleanup.cache_min_age_hours, 48);
        assert_eq!(config.cleanup.build_min_age_hours, 72);
        assert_eq!(config.cleanup.node_modules_min_age_hours, 720);
        assert_eq!(
            config.cleanup.scan_roots,
            vec![PathBuf::from("/home/guard-test")]
        );
        assert_eq!(
            config.cleanup.protected_paths,
            vec![
                PathBuf::from("/etc"),
                PathBuf::from("/home/guard-test/.ssh")
            ]
        );
    }

    #[test]
    fn new_schema_remains_strict_and_never_falls_back_on_invalid_values() {
        let unknown_field = r#"
schema_version = 1

[admission]
enabled = true
unexpected = true
"#;
        assert!(parse_config_text(unknown_field, Some(Path::new("/home/guard-test"))).is_err());

        let invalid_value = r#"
schema_version = 1

[monitor]
interval_seconds = "fast"
"#;
        assert!(parse_config_text(invalid_value, Some(Path::new("/home/guard-test"))).is_err());
    }

    #[test]
    fn malformed_known_legacy_value_is_rejected_instead_of_defaulted() {
        let legacy = r#"
version = 1

[workloads]
enabled = "yes"
"#;
        assert!(parse_config_text(legacy, Some(Path::new("/home/guard-test"))).is_err());
    }

    #[test]
    fn legacy_protected_paths_expand_only_home_prefix_or_absolute_paths() {
        let home = Path::new("/home/guard-test");
        assert_eq!(
            migrate_legacy_protected_path("~/.config", Some(home)).unwrap(),
            PathBuf::from("/home/guard-test/.config")
        );
        assert_eq!(
            migrate_legacy_protected_path("/var/lib/guardwsl", None).unwrap(),
            PathBuf::from("/var/lib/guardwsl")
        );
        assert!(migrate_legacy_protected_path("~other/.ssh", Some(home)).is_err());
        assert!(migrate_legacy_protected_path("relative/path", Some(home)).is_err());
        assert!(migrate_legacy_protected_path("~/.ssh", None).is_err());
    }
}
