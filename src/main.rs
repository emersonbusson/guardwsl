use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};
use guardwsl::admission::{AdmissionClass, CommandIntent};
use guardwsl::build_gate::{BuildGate, GateState};
use guardwsl::cleanup::{CleanupMode, CleanupReport, execute_cleanup, plan_cleanup};
use guardwsl::config::{ConfigStore, GuardConfig};
use guardwsl::fsutil::{atomic_write_private, default_state_dir, ensure_private_dir, read_private};
use guardwsl::history::AuditLog;
use guardwsl::host::{
    DiskPressure, HostSnapshot, PowerShellHostProbe, classify_disk, ensure_build_headroom,
};
use guardwsl::maintenance_lock::MaintenanceLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(
    name = "guard",
    version,
    about = "Simple protection against full disks and concurrent heavy builds in WSL2"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Shows physical disk, RAM, monitor, and active-build state.
    Status(OutputArgs),
    /// Removes only revalidated, regenerable caches and artifacts.
    Clean(CleanArgs),
    /// Enables, disables, or shows the heavy-build gate.
    Admission {
        #[command(subcommand)]
        command: AdmissionCommand,
    },
    /// Shows, initializes, or validates configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Runs a command under GuardWSL policy.
    Exec(ExecArgs),
    /// Runs diagnostics and always prints every check.
    Doctor(OutputArgs),
    /// Shows recent audit records.
    History(HistoryArgs),
    /// Internal entry point for tool shims.
    #[command(hide = true)]
    Shim(ShimArgs),
    /// Automatic user-scoped monitor.
    #[command(hide = true)]
    Monitor(MonitorArgs),
}

#[derive(Debug, Args)]
struct OutputArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CleanArgs {
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum AdmissionCommand {
    On,
    Off,
    Status(OutputArgs),
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Show,
    Init,
    Validate,
    #[command(hide = true)]
    Normalize,
}

#[derive(Debug, Args)]
struct ExecArgs {
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

#[derive(Debug, Args)]
struct ShimArgs {
    tool: OsString,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<OsString>,
}

#[derive(Debug, Args)]
struct HistoryArgs {
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MonitorArgs {
    #[arg(long)]
    once: bool,
}

fn main() -> std::process::ExitCode {
    let code = match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("guard: {error:#}");
            1
        }
    };
    std::process::ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1))
}

fn run() -> Result<i32> {
    match Cli::parse().command {
        Command::Status(args) => status(args),
        Command::Clean(args) => clean(args),
        Command::Admission { command } => admission(command),
        Command::Config { command } => config(command),
        Command::Exec(args) => execute_vector(args.command),
        Command::Doctor(args) => doctor(args),
        Command::History(args) => history(args),
        Command::Shim(args) => execute_tool(args.tool, args.args),
        Command::Monitor(args) => monitor(args),
    }
}

fn status(args: OutputArgs) -> Result<i32> {
    let store = ConfigStore::discover()?;
    let loaded = store.load_read_only();
    let config = loaded
        .as_ref()
        .map(|loaded| loaded.config.clone())
        .unwrap_or_default();
    let gate = BuildGate::for_current_user()
        .and_then(|gate| gate.state())
        .map(|state| state_name(state).to_owned());
    let host = probe_host(&config);
    let monitor = read_monitor_status(&config);
    let last_cleanup = read_cleanup_cycle();
    let monitor_report = monitor.as_ref().and_then(|status| {
        serde_json::to_value(status).ok().map(|mut value| {
            value["healthy"] = json!(status.healthy(&config));
            value
        })
    });
    let pressure = host
        .as_ref()
        .ok()
        .map(|snapshot| classify_disk(snapshot.volume_free_bytes, &config.disk));
    let build_preflight = match &host {
        Ok(snapshot) => match ensure_build_headroom(snapshot, &config) {
            Ok(()) => json!({"ok": true, "error": null}),
            Err(error) => json!({"ok": false, "error": error.to_string()}),
        },
        Err(error) => json!({"ok": false, "error": error.to_string()}),
    };
    let report = json!({
        "service": "guardwsl",
        "version": env!("CARGO_PKG_VERSION"),
        "configured": {
            "ok": loaded.is_ok(),
            "origin": loaded.as_ref().ok().map(|value| value.origin),
            "degraded": loaded.as_ref().ok().is_some_and(|value| value.degraded),
            "admission_enabled": config.admission.enabled,
            "cleanup_enabled": config.cleanup.enabled,
            "error": loaded.as_ref().err().map(ToString::to_string),
        },
        "admission": {
            "configured_enabled": config.admission.enabled,
            "effective_reachable": gate.is_ok(),
            "gate_state": gate.as_ref().ok(),
            "error": gate.as_ref().err().map(ToString::to_string),
        },
        "host": host.as_ref().ok(),
        "host_error": host.as_ref().err().map(ToString::to_string),
        "pressure": pressure.map(DiskPressure::as_str),
        "heavy_build_preflight": build_preflight,
        "monitor": monitor_report,
        "cleanup_policy": {
            "enabled": config.cleanup.enabled,
            "scan_roots": config.cleanup.scan_roots,
            "cache_min_age_hours": config.cleanup.cache_min_age_hours,
            "build_min_age_hours": config.cleanup.build_min_age_hours,
            "node_modules_min_age_hours": config.cleanup.node_modules_min_age_hours,
            "allowlist": [
                "npm_yarn_pnpm_cache",
                "cargo_cache",
                "go_cache",
                "target",
                ".next",
                ".turbo",
                ".vite",
                ".pytest_cache",
                ".mypy_cache",
                ".ruff_cache",
                "node_modules"
            ],
        },
        "last_cleanup": last_cleanup,
        "disk_protection_independent_from_admission": true,
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_status(&report);
    }
    let healthy = loaded.as_ref().is_ok_and(|value| !value.degraded)
        && host.is_ok()
        && gate.is_ok()
        && monitor
            .as_ref()
            .is_some_and(|status| status.healthy(&config));
    Ok(if healthy { 0 } else { 1 })
}

fn print_status(report: &Value) {
    let configured = &report["configured"];
    println!(
        "Guard: config={} | admission={} | gate={}",
        if configured["ok"].as_bool() == Some(true)
            && configured["degraded"].as_bool() != Some(true)
        {
            "ok"
        } else {
            "failed"
        },
        if configured["admission_enabled"].as_bool() == Some(true) {
            "enabled"
        } else {
            "disabled"
        },
        report["admission"]["gate_state"]
            .as_str()
            .unwrap_or("unavailable")
    );
    if let Some(host) = report["host"].as_object() {
        println!(
            "Disk {}: {} free of {} ({})",
            host["volume_root"].as_str().unwrap_or("?"),
            format_bytes(host["volume_free_bytes"].as_u64().unwrap_or(0)),
            format_bytes(host["volume_total_bytes"].as_u64().unwrap_or(0)),
            report["pressure"].as_str().unwrap_or("unknown"),
        );
        println!(
            "Windows RAM: {} available of {}",
            format_bytes(host["host_available_memory_bytes"].as_u64().unwrap_or(0)),
            format_bytes(host["host_total_memory_bytes"].as_u64().unwrap_or(0)),
        );
        println!(
            "WSL: {} at {} | sparse VHDX: {}",
            host["distro"].as_str().unwrap_or("?"),
            host["vhdx_path"].as_str().unwrap_or("?"),
            if host["vhdx_sparse"].as_bool() == Some(true) {
                "yes"
            } else {
                "no - deletion frees ext4 space but does not shrink the physical file"
            }
        );
        if report["heavy_build_preflight"]["ok"].as_bool() == Some(true) {
            println!("New heavy build: allowed by the current preflight");
        } else {
            println!(
                "New heavy build: blocked ({})",
                report["heavy_build_preflight"]["error"]
                    .as_str()
                    .unwrap_or("telemetry unavailable")
            );
        }
    } else {
        println!(
            "Host: unavailable ({})",
            report["host_error"].as_str().unwrap_or("unknown error")
        );
    }
    let policy = &report["cleanup_policy"];
    let roots = policy["scan_roots"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    println!(
        "Cleanup: {} | roots: {}",
        if policy["enabled"].as_bool() == Some(true) {
            "enabled"
        } else {
            "disabled"
        },
        roots
    );
    println!(
        "Allowlist: npm/Yarn/pnpm/Cargo/Go caches; target, .next, .turbo, .vite, Python caches, and node_modules"
    );
    if report["last_cleanup"].is_null() {
        println!("Last scan: not run yet");
    } else {
        println!(
            "Last scan: {} | actions={} | removed={} | safe skips={} | failures={}",
            report["last_cleanup"]["cleanup"]["mode"]
                .as_str()
                .unwrap_or("unknown"),
            report["last_cleanup"]["cleanup"]["actions"]
                .as_array()
                .map_or(0, Vec::len),
            format_bytes(
                report["last_cleanup"]["cleanup"]["deleted_logical_bytes"]
                    .as_u64()
                    .unwrap_or(0)
            ),
            report["last_cleanup"]["cleanup"]["planning_skips"]
                .as_array()
                .map_or(0, Vec::len),
            report["last_cleanup"]["cleanup"]["failures"]
                .as_u64()
                .unwrap_or(0),
        );
    }
    if report["monitor"].is_null() {
        println!("Monitor: no heartbeat");
    } else {
        println!(
            "Monitor: {} | last cycle={} | next maintenance={}",
            if report["monitor"]["healthy"].as_bool() == Some(true) {
                "active"
            } else {
                "stale/failed"
            },
            report["monitor"]["last_cycle_at"]
                .as_str()
                .unwrap_or("unknown"),
            report["monitor"]["next_maintenance_at"]
                .as_str()
                .unwrap_or("unknown")
        );
    }
}

fn clean(args: CleanArgs) -> Result<i32> {
    let loaded = ConfigStore::discover()?.load_read_only()?;
    if loaded.degraded && !args.dry_run {
        bail!("configuration is degraded; real cleanup is blocked")
    }
    let _lock = MaintenanceLock::acquire(Duration::from_secs(30))?;
    let snapshot = probe_host(&loaded.config)?;
    let pressure = classify_disk(snapshot.volume_free_bytes, &loaded.config.disk);
    let cycle = run_cleanup(
        &loaded.config,
        pressure,
        if args.dry_run {
            CleanupMode::DryRun
        } else {
            CleanupMode::Execute
        },
        Some(&snapshot),
    )?;
    print_cleanup(&cycle, args.json)?;
    Ok(if cycle.cleanup.succeeded() { 0 } else { 1 })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CleanupCycle {
    cleanup: CleanupReport,
    host_free_before_bytes: Option<u64>,
    host_free_after_bytes: Option<u64>,
    host_free_delta_bytes: Option<i64>,
}

fn run_cleanup(
    config: &GuardConfig,
    pressure: DiskPressure,
    mode: CleanupMode,
    host_before: Option<&HostSnapshot>,
) -> Result<CleanupCycle> {
    let audit = AuditLog::discover()?;
    let mut plan = plan_cleanup(config, pressure)?;
    if pressure != DiskPressure::Healthy
        && let Some(snapshot) = host_before
    {
        let logical_budget = config
            .disk
            .target_free_bytes
            .saturating_sub(snapshot.volume_free_bytes);
        let mut selected = 0_u64;
        plan.candidates.retain(|candidate| {
            if selected >= logical_budget {
                return false;
            }
            selected = selected.saturating_add(candidate.estimated_bytes);
            true
        });
    }
    let cleanup = execute_cleanup(config, &plan, mode, &audit)?;
    let host_after = if mode == CleanupMode::Execute {
        probe_host(config).ok()
    } else {
        None
    };
    let before_free = host_before.map(|snapshot| snapshot.volume_free_bytes);
    let after_free = host_after
        .as_ref()
        .map(|snapshot| snapshot.volume_free_bytes);
    let delta = before_free.zip(after_free).map(|(before, after)| {
        i64::try_from(after)
            .unwrap_or(i64::MAX)
            .saturating_sub(i64::try_from(before).unwrap_or(i64::MAX))
    });
    let cycle = CleanupCycle {
        cleanup,
        host_free_before_bytes: before_free,
        host_free_after_bytes: after_free,
        host_free_delta_bytes: delta,
    };
    let path = default_state_dir()?.join("cleanup-last.json");
    atomic_write_private(&path, &serde_json::to_vec_pretty(&cycle)?)?;
    Ok(cycle)
}

fn print_cleanup(cycle: &CleanupCycle, json_output: bool) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(cycle)?);
        return Ok(());
    }
    let report = &cycle.cleanup;
    println!(
        "Cleanup {:?}: {} planned; {} logically removed; {} failures",
        report.mode,
        format_bytes(report.planned_logical_bytes),
        format_bytes(report.deleted_logical_bytes),
        report.failures
    );
    for action in &report.actions {
        println!(
            "- {:?}: {} ({})",
            action.outcome,
            action.path.display(),
            action.detail
        );
    }
    if !report.planning_skips.is_empty() {
        println!(
            "Safe skips: {} (showing up to 20)",
            report.planning_skips.len()
        );
        for skip in report.planning_skips.iter().take(20) {
            println!("- preserved: {} ({})", skip.path.display(), skip.reason);
        }
    }
    if let Some(delta) = cycle.host_free_delta_bytes {
        println!(
            "Observed physical host delta: {}{}",
            if delta >= 0 { "+" } else { "" },
            format_bytes(delta.unsigned_abs())
        );
    }
    Ok(())
}

fn read_cleanup_cycle() -> Option<CleanupCycle> {
    let path = default_state_dir().ok()?.join("cleanup-last.json");
    let bytes = read_private(&path, 4 * 1024 * 1024).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn admission(command: AdmissionCommand) -> Result<i32> {
    let store = ConfigStore::discover()?;
    match command {
        AdmissionCommand::On => {
            let loaded = store.set_admission_enabled(true)?;
            println!(
                "Admission enabled; cleanup remains {}.",
                if loaded.config.cleanup.enabled {
                    "enabled"
                } else {
                    "disabled by configuration"
                }
            );
        }
        AdmissionCommand::Off => {
            let loaded = store.set_admission_enabled(false)?;
            println!(
                "Admission disabled; cleanup remains {}.",
                if loaded.config.cleanup.enabled {
                    "enabled"
                } else {
                    "disabled by configuration"
                }
            );
        }
        AdmissionCommand::Status(args) => {
            let loaded = store.load_read_only()?;
            let gate = BuildGate::for_current_user()?.state()?;
            let report = json!({
                "configured_enabled": loaded.config.admission.enabled,
                "effective_reachable": true,
                "gate_state": state_name(gate),
                "heavy_builds": "exclusive_wait",
                "tests_and_checks": "direct_not_gated",
                "installs": "direct_not_gated",
                "cleanup_independent": true,
            });
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "Admission: {} | gate: {} | tests/checks: direct and never gated",
                    if loaded.config.admission.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    state_name(gate)
                );
                println!("Cleanup: independent from admission.");
            }
        }
    }
    Ok(0)
}

fn config(command: ConfigCommand) -> Result<i32> {
    let store = ConfigStore::discover()?;
    match command {
        ConfigCommand::Show => {
            let loaded = store.load_read_only()?;
            print!("{}", loaded.config.to_toml()?);
            if loaded.degraded {
                eprintln!("# DEGRADED: {}", loaded.warnings.join("; "));
            }
        }
        ConfigCommand::Init => {
            if store.init_if_missing()? {
                println!("Configuration created: {}", store.paths().user.display());
            } else {
                println!(
                    "Configuration already exists: {}",
                    store.paths().user.display()
                );
            }
        }
        ConfigCommand::Validate => {
            let loaded = store.load_read_only()?;
            loaded.config.validate()?;
            if loaded.degraded {
                bail!(
                    "active configuration is invalid; effective source {:?}: {}",
                    loaded.origin,
                    loaded.warnings.join("; ")
                )
            }
            println!("Configuration is valid ({:?})", loaded.origin);
        }
        ConfigCommand::Normalize => {
            let loaded = store.load_read_only()?;
            if loaded.degraded {
                bail!("configuration is degraded; normalization is blocked")
            }
            store.save(&loaded.config)?;
            println!("Configuration normalized to schema v1.");
        }
    }
    Ok(0)
}

fn execute_vector(command: Vec<OsString>) -> Result<i32> {
    let (tool, args) = command.split_first().context("missing command")?;
    execute_tool(tool.clone(), args.to_vec())
}

fn execute_tool(tool: OsString, args: Vec<OsString>) -> Result<i32> {
    let intent = CommandIntent::classify(&tool, &args);
    let executable = resolve_tool(&tool)?;
    if intent.class != AdmissionClass::HeavyBuild {
        return run_child(&executable, &args, None, false);
    }
    if inherited_heavy_permit() {
        return run_child(&executable, &args, None, false);
    }
    let loaded = ConfigStore::discover()?.load_read_only()?;
    if loaded.degraded {
        bail!("configuration is degraded; protected execution is blocked")
    }
    let gate = loaded
        .config
        .admission
        .enabled
        .then(BuildGate::for_current_user)
        .transpose()?;
    match intent.class {
        AdmissionClass::HeavyBuild => {
            let wait = Duration::from_secs(loaded.config.admission.build_wait_seconds);
            let _guard = match &gate {
                Some(gate) => Some(gate.acquire_heavy(wait)?),
                None => None,
            };
            let _maintenance = MaintenanceLock::acquire_shared(wait)?;
            let snapshot = probe_host(&loaded.config)?;
            ensure_build_headroom(&snapshot, &loaded.config)?;
            let scratch = BuildScratch::new()?;
            let _marker = ActiveBuildMarker::create(&intent, &snapshot)?;
            run_child(&executable, &args, Some(&scratch.path), true)
        }
        AdmissionClass::TestOrCheck | AdmissionClass::Install | AdmissionClass::Other => {
            unreachable!("only heavy builds reach configuration and the gate")
        }
    }
}

fn resolve_tool(tool: &OsStr) -> Result<PathBuf> {
    let path = Path::new(tool);
    if path.components().count() > 1 {
        return Ok(path.to_path_buf());
    }
    let current = std::env::current_exe()
        .ok()
        .and_then(|path| fs::canonicalize(path).ok());
    let path_value = std::env::var_os("PATH").context("PATH is missing")?;
    for directory in std::env::split_paths(&path_value) {
        if directory.to_string_lossy().contains("guardwsl/shims") {
            continue;
        }
        for name in [tool.to_os_string(), {
            let mut value = tool.to_os_string();
            value.push(".exe");
            value
        }] {
            let candidate = directory.join(name);
            let Ok(metadata) = fs::metadata(&candidate) else {
                continue;
            };
            if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
            let canonical = fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
            if current.as_ref() == Some(&canonical) {
                continue;
            }
            return Ok(candidate);
        }
    }
    if let Some(candidate) = resolve_version_manager_tool(tool) {
        return Ok(candidate);
    }
    bail!("real executable not found for {}", tool.to_string_lossy())
}

fn resolve_version_manager_tool(tool: &OsStr) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())?;
    let mut roots = Vec::new();
    if let Some(nvm_bin) = std::env::var_os("NVM_BIN") {
        roots.push(PathBuf::from(nvm_bin));
    }
    let versions = home.join(".nvm/versions/node");
    if let Ok(entries) = fs::read_dir(versions) {
        let mut entries = entries
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        entries.sort();
        entries.reverse();
        roots.extend(entries.into_iter().map(|version| version.join("bin")));
    }
    roots.push(home.join(".bun/bin"));
    for root in roots {
        for name in [tool.to_os_string(), {
            let mut value = tool.to_os_string();
            value.push(".exe");
            value
        }] {
            let candidate = root.join(name);
            let Ok(metadata) = fs::metadata(&candidate) else {
                continue;
            };
            if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
                return Some(candidate);
            }
        }
    }
    None
}

fn run_child(
    executable: &Path,
    args: &[OsString],
    scratch: Option<&Path>,
    mark_heavy_permit: bool,
) -> Result<i32> {
    let mut command = ProcessCommand::new(executable);
    command.args(args);
    if mark_heavy_permit {
        command.env("GUARDWSL_HEAVY_ROOT_PID", std::process::id().to_string());
    }
    if let Some(scratch) = scratch {
        command
            .env("TMPDIR", scratch)
            .env("TMP", scratch)
            .env("TEMP", scratch)
            .env("GUARDWSL_BUILD_SCRATCH", scratch);
    }
    let status = command
        .status()
        .with_context(|| format!("failed to start {}", executable.display()))?;
    Ok(exit_code(status))
}

fn inherited_heavy_permit() -> bool {
    let Some(root_pid) = std::env::var_os("GUARDWSL_HEAVY_ROOT_PID")
        .and_then(|value| value.to_str().and_then(|value| value.parse::<u32>().ok()))
    else {
        return false;
    };
    if root_pid == std::process::id() || !process_has_ancestor(root_pid) {
        return false;
    }
    let Ok(path) = default_state_dir().map(|root| active_build_marker_path(&root, root_pid)) else {
        return false;
    };
    let Ok(bytes) = read_private(&path, 256 * 1024) else {
        return false;
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| value["pid"].as_u64())
        == Some(u64::from(root_pid))
}

fn process_has_ancestor(expected: u32) -> bool {
    let mut current = std::process::id();
    for _ in 0..256 {
        let Ok(stat) = fs::read(format!("/proc/{current}/stat")) else {
            return false;
        };
        let Some(parent) = parent_pid_from_stat(&stat) else {
            return false;
        };
        if parent == expected {
            return true;
        }
        if parent <= 1 || parent == current {
            return false;
        }
        current = parent;
    }
    false
}

fn parent_pid_from_stat(stat: &[u8]) -> Option<u32> {
    let stat = std::str::from_utf8(stat).ok()?;
    let after_name = stat.rsplit_once(") ")?.1;
    after_name.split_whitespace().nth(1)?.parse().ok()
}

fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    use std::os::unix::process::ExitStatusExt;
    status.signal().map_or(1, |signal| 128 + signal)
}

struct BuildScratch {
    path: PathBuf,
}

impl BuildScratch {
    fn new() -> Result<Self> {
        let runtime = runtime_dir()?;
        let path = runtime.join(format!("guardwsl-job-{}", std::process::id()));
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != effective_uid()
            {
                bail!("existing scratch directory has an unsafe identity")
            }
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(Self { path })
    }
}

impl Drop for BuildScratch {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == effective_uid()
        {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct ActiveBuildMarker {
    path: PathBuf,
}

impl ActiveBuildMarker {
    fn create(intent: &CommandIntent, snapshot: &HostSnapshot) -> Result<Self> {
        let path = active_build_marker_path(&default_state_dir()?, std::process::id());
        atomic_write_private(
            &path,
            &serde_json::to_vec_pretty(&json!({
                "pid": std::process::id(),
                "started_at": Utc::now(),
                "intent": intent,
                "host_at_admission": snapshot,
            }))?,
        )?;
        Ok(Self { path })
    }
}

fn active_build_marker_path(state_root: &Path, pid: u32) -> PathBuf {
    state_root.join(format!("active-build-{pid}.json"))
}

impl Drop for ActiveBuildMarker {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == effective_uid()
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn history(args: HistoryArgs) -> Result<i32> {
    let records = AuditLog::discover()?.tail(args.limit)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&records)?);
    } else if records.is_empty() {
        println!("History is empty");
    } else {
        for record in records {
            println!(
                "{} {:?} {} {}",
                record.at.to_rfc3339(),
                record.outcome,
                record.event,
                record.detail
            );
        }
    }
    Ok(0)
}

fn doctor(args: OutputArgs) -> Result<i32> {
    let mut checks = Vec::new();
    let store = ConfigStore::discover();
    let loaded = store
        .as_ref()
        .ok()
        .and_then(|store| store.load_read_only().ok());
    checks.push(json!({
        "name": "config",
        "ok": loaded.as_ref().is_some_and(|value| !value.degraded),
        "detail": loaded.as_ref().map(|value| json!({"origin":value.origin,"warnings":value.warnings})).unwrap_or_else(|| json!(store.as_ref().err().map(ToString::to_string))),
    }));
    let config = loaded
        .as_ref()
        .map(|loaded| loaded.config.clone())
        .unwrap_or_default();
    match BuildGate::for_current_user().and_then(|gate| gate.state()) {
        Ok(state) => checks.push(json!({"name":"build_gate","ok":true,"detail":state_name(state)})),
        Err(error) => {
            checks.push(json!({"name":"build_gate","ok":false,"detail":error.to_string()}))
        }
    }
    match probe_host(&config) {
        Ok(snapshot) => checks.push(json!({
            "name":"host_probe",
            "ok":true,
            "detail":{"volume":snapshot.volume_root,"free_bytes":snapshot.volume_free_bytes,"memory_free_bytes":snapshot.host_available_memory_bytes}
        })),
        Err(error) => checks.push(json!({"name":"host_probe","ok":false,"detail":error.to_string()})),
    }
    let monitor = read_monitor_status(&config);
    checks.push(json!({
        "name":"monitor",
        "ok":monitor.as_ref().is_some_and(|status| status.healthy(&config)),
        "detail":monitor,
    }));
    match AuditLog::discover().and_then(|log| log.tail(1)) {
        Ok(_) => checks.push(json!({"name":"audit","ok":true,"detail":"history is readable"})),
        Err(error) => checks.push(json!({"name":"audit","ok":false,"detail":error.to_string()})),
    }
    let ok = checks
        .iter()
        .all(|check| check["ok"].as_bool() == Some(true));
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"ok":ok,"checks":checks}))?
        );
    } else {
        for check in &checks {
            println!(
                "{} {:<14} {}",
                if check["ok"].as_bool() == Some(true) {
                    "OK"
                } else {
                    "FAIL"
                },
                check["name"].as_str().unwrap_or("unknown"),
                check["detail"]
            );
        }
    }
    Ok(if ok { 0 } else { 1 })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MonitorStatus {
    schema_version: u32,
    pid: u32,
    started_at: DateTime<Utc>,
    last_cycle_at: DateTime<Utc>,
    next_maintenance_at: DateTime<Utc>,
    last_cleanup_at: Option<DateTime<Utc>>,
    last_pressure: Option<DiskPressure>,
    last_error: Option<String>,
}

impl MonitorStatus {
    fn healthy(&self, config: &GuardConfig) -> bool {
        if self.schema_version != 1 || self.last_error.is_some() || !pid_alive(self.pid) {
            return false;
        }
        let age = Utc::now()
            .signed_duration_since(self.last_cycle_at)
            .num_seconds();
        age >= 0 && age <= i64::try_from(config.monitor.interval_seconds * 4).unwrap_or(i64::MAX)
    }
}

fn monitor(args: MonitorArgs) -> Result<i32> {
    let runtime = runtime_dir()?;
    let _singleton = MaintenanceLock::acquire_at(
        &runtime.join("guardwsl-monitor.lock"),
        Duration::from_secs(1),
    )
    .context("monitor is already active")?;
    let store = ConfigStore::discover()?;
    let loaded = store.load()?;
    if loaded.degraded {
        bail!("configuration is degraded; monitor failed closed")
    }
    let mut config = loaded.config;
    let started_at = Utc::now();
    let mut maintenance_interval = Duration::from_secs(config.monitor.maintenance_interval_seconds);
    let mut next_maintenance = Instant::now() + maintenance_interval;
    let mut next_pressure_cleanup = Instant::now();
    let mut last_cleanup_at = None;

    loop {
        let cycle_at = Utc::now();
        let mut last_error = None;
        let mut pressure = None;
        let config_ready = match store.load_read_only() {
            Ok(loaded) if !loaded.degraded => {
                let new_interval =
                    Duration::from_secs(loaded.config.monitor.maintenance_interval_seconds);
                if new_interval != maintenance_interval {
                    maintenance_interval = new_interval;
                    next_maintenance = Instant::now() + maintenance_interval;
                }
                config = loaded.config;
                true
            }
            Ok(_) => {
                last_error =
                    Some("configuration is degraded; cycle performed no mutation".to_owned());
                false
            }
            Err(error) => {
                last_error = Some(format!("invalid configuration: {error:#}"));
                false
            }
        };
        let host = config_ready.then(|| probe_host(&config));
        match host {
            Some(Ok(snapshot)) => {
                let current = classify_disk(snapshot.volume_free_bytes, &config.disk);
                pressure = Some(current);
                let pressure_due =
                    current != DiskPressure::Healthy && Instant::now() >= next_pressure_cleanup;
                let scheduled_due =
                    current == DiskPressure::Healthy && Instant::now() >= next_maintenance;
                if config.cleanup.enabled
                    && (pressure_due || scheduled_due)
                    && let Some(_maintenance) = MaintenanceLock::try_acquire()?
                {
                    match probe_host(&config) {
                        Ok(fresh_snapshot) => {
                            let fresh_pressure =
                                classify_disk(fresh_snapshot.volume_free_bytes, &config.disk);
                            pressure = Some(fresh_pressure);
                            match run_cleanup(
                                &config,
                                fresh_pressure,
                                CleanupMode::Execute,
                                Some(&fresh_snapshot),
                            ) {
                                Ok(cycle) if cycle.cleanup.succeeded() => {
                                    last_cleanup_at = Some(Utc::now());
                                    if fresh_pressure != DiskPressure::Healthy {
                                        next_pressure_cleanup = Instant::now()
                                            + pressure_cleanup_cooldown(fresh_pressure);
                                    } else {
                                        next_maintenance = Instant::now() + maintenance_interval;
                                    }
                                }
                                Ok(cycle) => {
                                    last_error = Some(format!(
                                        "cleanup finished with {} failures",
                                        cycle.cleanup.failures
                                    ));
                                }
                                Err(error) => last_error = Some(error.to_string()),
                            }
                        }
                        Err(error) => last_error = Some(error.to_string()),
                    }
                }
            }
            Some(Err(error)) => last_error = Some(error.to_string()),
            None => {}
        }
        let until_maintenance = next_maintenance.saturating_duration_since(Instant::now());
        let status = MonitorStatus {
            schema_version: 1,
            pid: std::process::id(),
            started_at,
            last_cycle_at: cycle_at,
            next_maintenance_at: Utc::now()
                + chrono::Duration::from_std(until_maintenance).unwrap_or_default(),
            last_cleanup_at,
            last_pressure: pressure,
            last_error,
        };
        write_monitor_status(&status)?;
        if args.once {
            return Ok(if status.last_error.is_none() { 0 } else { 1 });
        }
        thread::sleep(Duration::from_secs(config.monitor.interval_seconds));
    }
}

fn read_monitor_status(_config: &GuardConfig) -> Option<MonitorStatus> {
    let path = default_state_dir().ok()?.join("monitor-status.json");
    let bytes = read_private(&path, 256 * 1024).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_monitor_status(status: &MonitorStatus) -> Result<()> {
    let path = default_state_dir()?.join("monitor-status.json");
    atomic_write_private(&path, &serde_json::to_vec_pretty(status)?)
}

fn probe_host(config: &GuardConfig) -> Result<HostSnapshot> {
    let timeout = Duration::from_secs(config.disk.host_probe_timeout_seconds);
    let snapshot = PowerShellHostProbe::new(timeout).probe()?;
    snapshot.require_fresh(timeout.saturating_mul(2))?;
    Ok(snapshot)
}

fn runtime_dir() -> Result<PathBuf> {
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", effective_uid())));
    ensure_private_dir(&path)?;
    Ok(path)
}

fn effective_uid() -> libc::uid_t {
    // SAFETY: geteuid has no arguments, preconditions, or failure mode.
    unsafe { libc::geteuid() }
}

fn pid_alive(pid: u32) -> bool {
    PathBuf::from("/proc").join(pid.to_string()).exists()
}

fn state_name(state: GateState) -> &'static str {
    match state {
        GateState::Idle => "idle",
        GateState::HeavyBuildActive => "build_active",
    }
}

fn pressure_cleanup_cooldown(pressure: DiskPressure) -> Duration {
    Duration::from_secs(match pressure {
        DiskPressure::Healthy => 15 * 60,
        DiskPressure::Pressure => 15 * 60,
        DiskPressure::Critical => 5 * 60,
        DiskPressure::Emergency => 60,
    })
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use guardwsl::config::ConfigOrigin;

    #[test]
    fn child_exit_status_is_preserved() {
        let status = ProcessCommand::new("sh")
            .args(["-c", "exit 23"])
            .status()
            .unwrap();
        assert_eq!(exit_code(status), 23);
    }

    #[test]
    fn gate_state_names_are_stable_for_status_and_scripts() {
        assert_eq!(state_name(GateState::Idle), "idle");
        assert_eq!(state_name(GateState::HeavyBuildActive), "build_active");
    }

    #[test]
    fn disabled_admission_does_not_imply_disabled_cleanup() {
        let mut config = GuardConfig::default();
        config.admission.enabled = false;
        assert!(config.cleanup.enabled);
    }

    #[test]
    fn config_origin_is_serializable() {
        assert_eq!(
            serde_json::to_value(ConfigOrigin::Default).unwrap(),
            json!("default")
        );
    }

    #[test]
    fn proc_stat_parent_parser_uses_the_field_after_the_last_name_parenthesis() {
        assert_eq!(
            parent_pid_from_stat(b"123 (worker ) name) S 42 1 2 3"),
            Some(42)
        );
        assert_eq!(parent_pid_from_stat(b"invalid"), None);
    }
}
