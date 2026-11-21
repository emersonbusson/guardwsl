use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use guardwsl::cleanup::{
    CacheAgePolicy, CleanupKind, CleanupMode, CleanupReport, execute_cleanup,
    plan_cleanup_with_cache_policy,
};
use guardwsl::config::{ConfigStore, GuardConfig, needs_normalization};
use guardwsl::fsutil::{atomic_write_private, default_state_dir, ensure_private_dir, read_private};
use guardwsl::history::AuditLog;
use guardwsl::host::{
    DiskPressure, HostPlatform, HostSnapshot, PowerShellHostProbe, classify_disk,
    compaction_advised, current_platform, host_reclaim_possible, probe_linux_disk,
    vhdx_occupancy_ratio,
};
use guardwsl::maintenance_lock::MaintenanceLock;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const NO_SAFE_RECLAIM: &str =
    "Disk space is critically low and the last cleanup found no safe space to reclaim.";

#[derive(Debug, Parser)]
#[command(
    name = "guard",
    version = concat!(
        env!("CARGO_PKG_VERSION"),
        " (commit ",
        env!("GUARD_GIT_COMMIT"),
        ")"
    ),
    long_version = concat!(
        env!("CARGO_PKG_VERSION"),
        " (commit ",
        env!("GUARD_GIT_COMMIT"),
        ")"
    ),
    about = "Safe cache cleanup and host-disk pressure monitoring for Linux and WSL2"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Shows physical host disk and monitor state.
    Status(OutputArgs),
    /// Removes only revalidated, regenerable caches and artifacts.
    Clean(CleanArgs),
    /// Shows, initializes, or validates configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Runs diagnostics and always prints every check.
    Doctor(OutputArgs),
    /// Shows recent audit records.
    History(HistoryArgs),
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
    /// Include recent allowlisted caches; retain build and dependency age limits.
    #[arg(long)]
    all_caches: bool,
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
        Command::Config { command } => config(command),
        Command::Doctor(args) => doctor(args),
        Command::History(args) => history(args),
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
    let host = probe_host(&config);
    let monitor = read_monitor_status(&config);
    let last_cleanup = read_cleanup_cycle();
    let installation = read_install_metadata();
    let monitor_report = monitor.as_ref().and_then(|status| {
        serde_json::to_value(status).ok().map(|mut value| {
            value["healthy"] = json!(status.healthy(&config));
            value
        })
    });
    let pressure = host.as_ref().ok().map(|snapshot| {
        classify_disk(
            snapshot.volume_free_bytes,
            snapshot.volume_total_bytes,
            &config.disk,
        )
    });
    let reclaim_possible = host.as_ref().ok().map(host_reclaim_possible);
    let occupancy = host.as_ref().ok().and_then(vhdx_occupancy_ratio);
    let advise_compaction = compaction_advised(
        host.as_ref().ok(),
        pressure,
        last_cleanup
            .as_ref()
            .map(|cycle| cycle.cleanup.deleted_logical_bytes)
            .unwrap_or(0),
        last_cleanup
            .as_ref()
            .and_then(|cycle| cycle.host_free_delta_bytes),
    );
    let report = json!({
        "service": "guardwsl",
        "version": env!("CARGO_PKG_VERSION"),
        "commit": env!("GUARD_GIT_COMMIT"),
        "commit_short": env!("GUARD_GIT_COMMIT_SHORT"),
        "installed_at": installation.as_ref().map(|value| value.installed_at.as_str()),
        "installed_version": installation.as_ref().and_then(|value| value.version.as_deref()),
        "installed_commit": installation.as_ref().and_then(|value| value.commit.as_deref()),
        "installed_commit_short": installation.as_ref().and_then(|value| value.commit_short.as_deref()),
        "configured": {
            "ok": loaded.is_ok(),
            "origin": loaded.as_ref().ok().map(|value| value.origin),
            "degraded": loaded.as_ref().ok().is_some_and(|value| value.degraded),
            "cleanup_enabled": config.cleanup.enabled,
            "error": loaded.as_ref().err().map(ToString::to_string),
        },
        "host": host.as_ref().ok(),
        "host_error": host.as_ref().err().map(ToString::to_string),
        "pressure": pressure.map(DiskPressure::as_str),
        "host_reclaim_possible": reclaim_possible,
        "vhdx_occupancy_ratio": occupancy,
        "compaction_advised": advise_compaction,
        "monitor": monitor_report,
        "cleanup_policy": {
            "enabled": config.cleanup.enabled,
            "scan_roots": config.cleanup.scan_roots,
            "cache_min_age_hours": config.cleanup.cache_min_age_hours,
            "build_min_age_hours": config.cleanup.build_min_age_hours,
            "node_modules_min_age_hours": config.cleanup.node_modules_min_age_hours,
            "allowlist": CleanupKind::allowlist_names(),
        },
        "last_cleanup": last_cleanup,
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_status(&report);
    }
    let healthy = loaded.as_ref().is_ok_and(|value| !value.degraded)
        && host.is_ok()
        && monitor
            .as_ref()
            .is_some_and(|status| status.healthy(&config));
    Ok(if healthy { 0 } else { 1 })
}

fn print_status(report: &Value) {
    let configured = &report["configured"];
    println!(
        "Guard: config={}",
        if configured["ok"].as_bool() == Some(true)
            && configured["degraded"].as_bool() != Some(true)
        {
            "ok"
        } else {
            "failed"
        }
    );
    println!(
        "Version: {} (commit {})",
        report["version"].as_str().unwrap_or("unknown"),
        report["commit_short"].as_str().unwrap_or("unknown")
    );
    println!(
        "Installed: {}",
        report["installed_at"]
            .as_str()
            .unwrap_or("unknown (installation record missing or invalid)")
    );
    if let Some(host) = report["host"].as_object() {
        println!(
            "Host disk {}: {} free of {} ({})",
            host["volume_root"].as_str().unwrap_or("?"),
            format_bytes(host["volume_free_bytes"].as_u64().unwrap_or(0)),
            format_bytes(host["volume_total_bytes"].as_u64().unwrap_or(0)),
            report["pressure"].as_str().unwrap_or("unknown"),
        );
        match host["platform"].as_str() {
            Some("wsl2") => {
                println!(
                    "WSL2/Windows backing volume: {} at {} | sparse VHDX: {}",
                    host["distro"].as_str().unwrap_or("?"),
                    host["vhdx_path"].as_str().unwrap_or("?"),
                    if host["vhdx_sparse"].as_bool() == Some(true) {
                        "yes"
                    } else {
                        "no - deletion frees ext4 space but does not shrink the physical file"
                    }
                );
                if let Some(size) = host["vhdx_size_bytes"].as_u64() {
                    println!(
                        "WSL2 VHDX occupancy: {} of {} host volume ({:.1}%)",
                        format_bytes(size),
                        format_bytes(host["volume_total_bytes"].as_u64().unwrap_or(0)),
                        report["vhdx_occupancy_ratio"].as_f64().unwrap_or(0.0) * 100.0
                    );
                }
            }
            Some("linux") => println!(
                "Linux local filesystem: {}",
                host["volume_root"].as_str().unwrap_or("?")
            ),
            _ => println!("Host platform: unknown"),
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
    let allowlist = policy["allowlist"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| CleanupKind::allowlist_names().join(", "));
    println!("Allowlist: {allowlist}");
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
    if report["monitor"]["no_safe_reclaim"].as_bool() == Some(true) {
        println!("Warning: {NO_SAFE_RECLAIM}");
    }
    if report["compaction_advised"].as_bool() == Some(true) {
        println!(
            "WARN: host space cannot be reclaimed by cleanup (non-sparse VHDX). \
             Offline compaction is required; see docs/SAFETY.md."
        );
    } else if report["host_reclaim_possible"].as_bool() == Some(false) {
        println!(
            "Note: non-sparse VHDX - cleanup prevents further growth but does not shrink the physical file."
        );
    }
}

fn clean(args: CleanArgs) -> Result<i32> {
    let loaded = ConfigStore::discover()?.load_read_only()?;
    if loaded.degraded && !args.dry_run {
        bail!("configuration is degraded; real cleanup is blocked")
    }
    let _lock = MaintenanceLock::acquire(Duration::from_secs(30))?;
    let host = probe_host(&loaded.config);
    let host_error = host.as_ref().err().map(|error| format!("{error:#}"));
    let snapshot = host.as_ref().ok();
    let pressure = cleanup_pressure(&loaded.config, snapshot);
    let cycle = run_cleanup(
        &loaded.config,
        pressure,
        if args.dry_run {
            CleanupMode::DryRun
        } else {
            CleanupMode::Execute
        },
        snapshot,
        if args.all_caches {
            CacheAgePolicy::IncludeRecent
        } else {
            CacheAgePolicy::Retention
        },
        host_error.as_deref(),
    )?;
    print_cleanup(&cycle, args.json)?;
    Ok(
        if cycle.cleanup.succeeded() && cycle.trim_error.is_none() && host_error.is_none() {
            0
        } else {
            1
        },
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CleanupCycle {
    cleanup: CleanupReport,
    #[serde(default)]
    cache_age_policy: CacheAgePolicy,
    host_free_before_bytes: Option<u64>,
    host_free_after_bytes: Option<u64>,
    host_free_delta_bytes: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_telemetry_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trim_error: Option<String>,
    /// None means host reclaim is unknown. False means a non-sparse VHDX.
    #[serde(default)]
    host_reclaim_possible: Option<bool>,
    #[serde(default)]
    vhdx_size_bytes: Option<u64>,
    #[serde(default)]
    vhdx_occupancy_ratio: Option<f64>,
    /// Offline VHDX compaction is required to recover host space.
    #[serde(default)]
    compaction_advised: bool,
}

fn below_target(config: &GuardConfig, snapshot: &HostSnapshot) -> bool {
    snapshot.volume_free_bytes
        < config
            .disk
            .effective_thresholds(snapshot.volume_total_bytes)
            .target_free_bytes
}

// Unknown host pressure never becomes an invented free-space measurement.
// Retention cleanup can still run independently with conservative pressure.
fn cleanup_pressure(config: &GuardConfig, snapshot: Option<&HostSnapshot>) -> DiskPressure {
    snapshot.map_or(DiskPressure::Pressure, |snapshot| {
        classify_disk(
            snapshot.volume_free_bytes,
            snapshot.volume_total_bytes,
            &config.disk,
        )
    })
}

/// Bound a plan by the space deletion can actually reclaim toward `target`.
///
/// The budget is host free vs host target only when deletion returns host
/// blocks (native Linux, or a sparse VHDX). On a non-sparse VHDX that budget
/// is unreachable and would under-clean under pressure. `Emergency` skips it
/// entirely: reclaim what is safe now, before any out-of-scope recovery.
fn apply_logical_budget(
    plan: &mut guardwsl::cleanup::CleanupPlan,
    config: &GuardConfig,
    snapshot: &HostSnapshot,
    pressure: DiskPressure,
    need_space: bool,
    reclaim_possible: bool,
) {
    if !need_space || !reclaim_possible || pressure == DiskPressure::Emergency {
        return;
    }
    let logical_budget = config
        .disk
        .effective_thresholds(snapshot.volume_total_bytes)
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

fn run_cleanup(
    config: &GuardConfig,
    pressure: DiskPressure,
    mode: CleanupMode,
    host_before: Option<&HostSnapshot>,
    cache_policy: CacheAgePolicy,
    host_telemetry_error: Option<&str>,
) -> Result<CleanupCycle> {
    let audit = AuditLog::discover()?;
    let need_space = pressure != DiskPressure::Healthy
        || host_before.is_some_and(|snapshot| below_target(config, snapshot));
    let mut plan = plan_cleanup_with_cache_policy(config, pressure, need_space, cache_policy)?;
    // A host-free budget is only meaningful when deletion can return host
    // blocks. On a non-sparse VHDX that budget is unreachable and would
    // under-clean exactly when pressure is highest; max_actions_per_cycle and
    // risk order still bound the plan.
    let reclaim_possible = host_before.is_none_or(host_reclaim_possible);
    if cache_policy == CacheAgePolicy::Retention
        && let Some(snapshot) = host_before
    {
        apply_logical_budget(
            &mut plan,
            config,
            snapshot,
            pressure,
            need_space,
            reclaim_possible,
        );
    }
    let cleanup = execute_cleanup(config, &plan, mode, &audit)?;
    let trim_error =
        if mode == CleanupMode::Execute && (cleanup.deleted_logical_bytes > 0 || need_space) {
            discard_guest_blocks()
                .err()
                .map(|error| format!("{error:#}"))
        } else {
            None
        };
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
    let identity = host_before.or(host_after.as_ref());
    let advise_compaction = compaction_advised(
        identity,
        Some(pressure),
        cleanup.deleted_logical_bytes,
        delta,
    );
    let cycle = CleanupCycle {
        cache_age_policy: cache_policy,
        host_reclaim_possible: identity.map(host_reclaim_possible),
        vhdx_size_bytes: identity.and_then(|snapshot| snapshot.vhdx_size_bytes),
        vhdx_occupancy_ratio: identity.and_then(vhdx_occupancy_ratio),
        compaction_advised: advise_compaction,
        cleanup,
        host_free_before_bytes: before_free,
        host_free_after_bytes: after_free,
        host_free_delta_bytes: delta,
        host_telemetry_error: host_telemetry_error.map(str::to_owned),
        trim_error,
    };
    let path = default_state_dir()?.join("cleanup-last.json");
    atomic_write_private(&path, &serde_json::to_vec_pretty(&cycle)?)?;
    Ok(cycle)
}

fn discard_guest_blocks() -> Result<()> {
    let (program, args): (&str, &[&str]) = if Path::new("/usr/bin/sudo").is_file() {
        ("/usr/bin/sudo", &["-n", "/usr/sbin/fstrim", "/"])
    } else {
        ("/usr/sbin/fstrim", &["/"])
    };
    discard_guest_blocks_with(program, args)
}

fn discard_guest_blocks_with(program: &str, args: &[&str]) -> Result<()> {
    let output = guardwsl::host::run_bounded_trim(program, args, Duration::from_secs(60))
        .context("guest block discard did not complete")?;
    if !output.status.success() || output.stdout_truncated || output.stderr_truncated {
        bail!("guest block discard failed ({})", output.status)
    }
    Ok(())
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
    if let Some(error) = &cycle.trim_error {
        println!("WARN: {error}");
    }
    if let Some(error) = &cycle.host_telemetry_error {
        println!("WARN: host telemetry unavailable; safe cleanup used no host target: {error}");
    }
    if let Some(ratio) = cycle.vhdx_occupancy_ratio {
        println!("VHDX occupancy: {:.1}% of the host volume", ratio * 100.0);
    }
    if cycle.compaction_advised {
        println!(
            "WARN: host space cannot be reclaimed by cleanup (non-sparse VHDX). \
             Offline compaction is required; see docs/SAFETY.md."
        );
    } else if cycle.host_reclaim_possible == Some(false) {
        println!(
            "Note: non-sparse VHDX - cleanup prevents further growth but does not shrink the physical file."
        );
    }
    Ok(())
}

fn read_cleanup_cycle() -> Option<CleanupCycle> {
    let path = default_state_dir().ok()?.join("cleanup-last.json");
    let bytes = read_private(&path, 4 * 1024 * 1024).ok()?;
    serde_json::from_slice(&bytes).ok()
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
            let path = &store.paths().user;
            if path.exists() {
                let bytes = read_private(path, 256 * 1024)?;
                let text = std::str::from_utf8(&bytes).context("configuration is not UTF-8")?;
                if !needs_normalization(text)? {
                    // Leave user comments and formatting alone.
                    println!("Configuration is already normalized; file left untouched.");
                    return Ok(0);
                }
            }
            store.save(&loaded.config)?;
            println!("Configuration normalized to schema v1.");
        }
    }
    Ok(0)
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
                record.at, record.outcome, record.event, record.detail
            );
        }
    }
    Ok(0)
}

fn doctor(args: OutputArgs) -> Result<i32> {
    let mut checks = Vec::new();
    let mut warnings = Vec::new();
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
    let host_snapshot = match probe_host(&config) {
        Ok(snapshot) => {
            checks.push(json!({
                "name":"host_probe",
                "ok":true,
                "detail":{
                    "volume":snapshot.volume_root,
                    "free_bytes":snapshot.volume_free_bytes,
                    "vhdx_size_bytes":snapshot.vhdx_size_bytes,
                    "host_reclaim_possible":host_reclaim_possible(&snapshot),
                }
            }));
            Some(snapshot)
        }
        Err(error) => {
            checks.push(json!({"name":"host_probe","ok":false,"detail":error.to_string()}));
            None
        }
    };
    // Boot integration: the monitor must come up with WSL2, not only at login.
    checks.push(boot_integration_check());
    let monitor = read_monitor_status(&config);
    if monitor
        .as_ref()
        .is_some_and(|status| status.no_safe_reclaim)
    {
        warnings.push(NO_SAFE_RECLAIM.to_owned());
    }
    checks.push(json!({
        "name":"monitor",
        "ok":monitor.as_ref().is_some_and(|status| status.healthy(&config)),
        "detail":monitor,
    }));
    match AuditLog::discover().and_then(|log| log.tail(1)) {
        Ok(_) => checks.push(json!({"name":"audit","ok":true,"detail":"history is readable"})),
        Err(error) => checks.push(json!({"name":"audit","ok":false,"detail":error.to_string()})),
    }
    let pressure = host_snapshot.as_ref().map(|snapshot| {
        classify_disk(
            snapshot.volume_free_bytes,
            snapshot.volume_total_bytes,
            &config.disk,
        )
    });
    let last_cleanup = read_cleanup_cycle();
    if compaction_advised(
        host_snapshot.as_ref(),
        pressure,
        last_cleanup
            .as_ref()
            .map(|cycle| cycle.cleanup.deleted_logical_bytes)
            .unwrap_or(0),
        last_cleanup
            .as_ref()
            .and_then(|cycle| cycle.host_free_delta_bytes),
    ) {
        warnings.push(
            "host space cannot be reclaimed by cleanup (non-sparse VHDX); offline compaction is required"
                .to_owned(),
        );
    } else if host_snapshot
        .as_ref()
        .is_some_and(|snapshot| !host_reclaim_possible(snapshot))
    {
        warnings.push(
            "non-sparse VHDX: cleanup prevents further growth but does not shrink the physical file"
                .to_owned(),
        );
    }
    let ok = checks
        .iter()
        .all(|check| check["ok"].as_bool() == Some(true));
    let version = env!("CARGO_PKG_VERSION");
    let commit = env!("GUARD_GIT_COMMIT");
    let commit_short = env!("GUARD_GIT_COMMIT_SHORT");
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": ok,
                "version": version,
                "commit": commit,
                "commit_short": commit_short,
                "warnings": warnings,
                "checks": checks
            }))?
        );
    } else {
        println!("Version: {version} (commit {commit_short})");
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
        for warning in &warnings {
            println!("WARN {warning}");
        }
    }
    Ok(if ok { 0 } else { 1 })
}

/// Verifies the monitor starts with WSL2: user systemd, linger, and the unit.
fn boot_integration_check() -> Value {
    let mut detail = serde_json::Map::new();
    let mut ok = true;

    let systemd_enabled = std::fs::read_to_string("/etc/wsl.conf")
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .any(|line| line == "systemd=true")
        })
        .unwrap_or(false);
    // On native Linux there is no wsl.conf; treat that as not applicable.
    let on_wsl = current_platform().is_ok_and(|platform| platform == HostPlatform::Wsl2);
    let systemd_ok = !on_wsl || systemd_enabled;
    if !systemd_ok {
        ok = false;
    }
    detail.insert(
        "systemd".to_owned(),
        json!({"ok": systemd_ok, "wsl2": on_wsl, "enabled": systemd_enabled}),
    );

    let linger_enabled = std::fs::metadata(format!(
        "/var/lib/systemd/linger/{}",
        std::env::var("USER").unwrap_or_default()
    ))
    .is_ok_and(|metadata| metadata.is_file());
    if !linger_enabled {
        ok = false;
    }
    detail.insert("linger".to_owned(), json!({"ok": linger_enabled}));

    let unit_enabled = std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", "guardwsl.service"])
        .output()
        .map(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "enabled"
        })
        .unwrap_or(false);
    if !unit_enabled {
        ok = false;
    }
    detail.insert(
        "unit".to_owned(),
        json!({"ok": unit_enabled, "name": "guardwsl.service"}),
    );

    json!({
        "name": "boot",
        "ok": ok,
        "detail": detail,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MonitorStatus {
    schema_version: u32,
    pid: u32,
    started_at: Timestamp,
    last_cycle_at: Timestamp,
    next_maintenance_at: Timestamp,
    last_cleanup_at: Option<Timestamp>,
    last_pressure: Option<DiskPressure>,
    last_error: Option<String>,
    #[serde(default)]
    no_safe_reclaim: bool,
    #[serde(default)]
    host_reclaim_possible: Option<bool>,
    #[serde(default)]
    compaction_advised: bool,
}

impl MonitorStatus {
    fn healthy(&self, config: &GuardConfig) -> bool {
        if self.schema_version != 1 || self.last_error.is_some() || !pid_alive(self.pid) {
            return false;
        }
        let age = Timestamp::now()
            .duration_since(self.last_cycle_at)
            .as_secs();
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
    let started_at = Timestamp::now();
    let mut maintenance_interval = Duration::from_secs(config.monitor.maintenance_interval_seconds);
    let mut next_maintenance = Instant::now() + maintenance_interval;
    let mut last_pressure_cleanup: Option<Instant> = None;
    let mut last_cleanup_at = None;
    let mut cleanup_error: Option<String> = None;
    let mut no_safe_reclaim = false;

    loop {
        let mut last_error = cleanup_error.clone();
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
        match host.as_ref() {
            Some(Ok(snapshot)) => {
                let current = classify_disk(
                    snapshot.volume_free_bytes,
                    snapshot.volume_total_bytes,
                    &config.disk,
                );
                pressure = Some(current);
                if !matches!(current, DiskPressure::Critical | DiskPressure::Emergency) {
                    no_safe_reclaim = false;
                }
                let need_space =
                    current != DiskPressure::Healthy || below_target(&config, snapshot);
                let pressure_due = pressure_cleanup_due(need_space, last_pressure_cleanup, current);
                let scheduled_due = !need_space && Instant::now() >= next_maintenance;
                if config.cleanup.enabled
                    && (pressure_due || scheduled_due)
                    && let Some(_maintenance) = MaintenanceLock::try_acquire()?
                {
                    match probe_host(&config) {
                        Ok(fresh_snapshot) => {
                            let fresh_pressure = classify_disk(
                                fresh_snapshot.volume_free_bytes,
                                fresh_snapshot.volume_total_bytes,
                                &config.disk,
                            );
                            pressure = Some(fresh_pressure);
                            match run_cleanup(
                                &config,
                                fresh_pressure,
                                CleanupMode::Execute,
                                Some(&fresh_snapshot),
                                CacheAgePolicy::Retention,
                                None,
                            ) {
                                Ok(cycle) if cycle.cleanup.succeeded() => {
                                    last_cleanup_at = Some(Timestamp::now());
                                    let after_pressure = cycle.host_free_after_bytes.map_or(
                                        fresh_pressure,
                                        |free| {
                                            classify_disk(
                                                free,
                                                fresh_snapshot.volume_total_bytes,
                                                &config.disk,
                                            )
                                        },
                                    );
                                    no_safe_reclaim = matches!(
                                        after_pressure,
                                        DiskPressure::Critical | DiskPressure::Emergency
                                    ) && cycle.cleanup.deleted_logical_bytes == 0
                                        && cycle
                                            .host_free_delta_bytes
                                            .is_none_or(|delta| delta <= 0);
                                    cleanup_error = cycle
                                        .cleanup
                                        .process_inspection_blocked()
                                        .then(|| {
                                            "cleanup could not safely inspect a same-user process"
                                                .to_owned()
                                        })
                                        .or_else(|| cycle.trim_error.clone());
                                    last_error = cleanup_error.clone();
                                    let still_short = fresh_pressure != DiskPressure::Healthy
                                        || below_target(&config, &fresh_snapshot);
                                    if still_short {
                                        last_pressure_cleanup = Some(Instant::now());
                                    } else {
                                        last_pressure_cleanup = None;
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
                        Err(error) => {
                            let message = format!("host telemetry unavailable: {error:#}");
                            pressure = None;
                            run_unverified_cleanup(
                                &config,
                                &message,
                                &mut last_pressure_cleanup,
                                &mut last_cleanup_at,
                                &mut cleanup_error,
                            );
                            last_error = Some(telemetry_failure_message(
                                &message,
                                cleanup_error.as_deref(),
                            ));
                        }
                    }
                }
            }
            Some(Err(error)) => {
                let message = format!("host telemetry unavailable: {error:#}");
                // A missing measurement blocks claims about host capacity, but
                // must not disable independently safe retention cleanup.
                if unknown_host_cleanup_due(config.cleanup.enabled, last_pressure_cleanup)
                    && let Some(_maintenance) = MaintenanceLock::try_acquire()?
                {
                    run_unverified_cleanup(
                        &config,
                        &message,
                        &mut last_pressure_cleanup,
                        &mut last_cleanup_at,
                        &mut cleanup_error,
                    );
                }
                last_error = Some(telemetry_failure_message(
                    &message,
                    cleanup_error.as_deref(),
                ));
            }
            None => {}
        }
        let until_maintenance = next_maintenance.saturating_duration_since(Instant::now());
        let latest_cycle = read_cleanup_cycle();
        let advise_compaction = compaction_advised(
            host.as_ref().and_then(|result| result.as_ref().ok()),
            pressure,
            latest_cycle
                .as_ref()
                .map(|cycle| cycle.cleanup.deleted_logical_bytes)
                .unwrap_or(0),
            latest_cycle
                .as_ref()
                .and_then(|cycle| cycle.host_free_delta_bytes),
        );
        let status = MonitorStatus {
            schema_version: 1,
            pid: std::process::id(),
            started_at,
            last_cycle_at: Timestamp::now(),
            next_maintenance_at: Timestamp::now().checked_add(until_maintenance)?,
            last_cleanup_at,
            last_pressure: pressure,
            no_safe_reclaim: no_safe_reclaim && pressure.is_some() && last_error.is_none(),
            last_error,
            host_reclaim_possible: host
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .map(host_reclaim_possible),
            compaction_advised: advise_compaction,
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

#[derive(Debug, Deserialize)]
struct InstallMetadata {
    installed_at: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    commit_short: Option<String>,
}

fn read_install_metadata() -> Option<InstallMetadata> {
    let path = default_state_dir().ok()?.join("install.json");
    let bytes = read_private(&path, 64 * 1024).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_monitor_status(status: &MonitorStatus) -> Result<()> {
    let path = default_state_dir()?.join("monitor-status.json");
    atomic_write_private(&path, &serde_json::to_vec_pretty(status)?)
}

fn probe_host(config: &GuardConfig) -> Result<HostSnapshot> {
    let timeout = Duration::from_secs(config.disk.host_probe_timeout_seconds);
    let snapshot = match current_platform()? {
        HostPlatform::Linux => probe_linux_disk(&config.cleanup.scan_roots)?,
        HostPlatform::Wsl2 => match guardwsl::host_fallback::probe_verified_drvfs() {
            Ok(snapshot) => snapshot,
            Err(fallback_error) => {
                let mut snapshot =
                    PowerShellHostProbe::new(timeout).probe().with_context(|| {
                        format!("verified DrvFs fallback unavailable: {fallback_error:#}")
                    })?;
                snapshot.source = Some("powershell".to_owned());
                // Failure to save a fallback never changes a valid live probe.
                let _ = guardwsl::host_fallback::remember_wsl_host(&snapshot);
                snapshot
            }
        },
    };
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

fn pressure_cleanup_cooldown(pressure: DiskPressure) -> Duration {
    Duration::from_secs(match pressure {
        DiskPressure::Healthy => 15 * 60,
        DiskPressure::Pressure => 2 * 60,
        DiskPressure::Critical => 60,
        DiskPressure::Emergency => 30,
    })
}

fn pressure_cleanup_due(need_space: bool, last: Option<Instant>, pressure: DiskPressure) -> bool {
    need_space && last.is_none_or(|last| last.elapsed() >= pressure_cleanup_cooldown(pressure))
}

fn unknown_host_cleanup_due(enabled: bool, last: Option<Instant>) -> bool {
    enabled && pressure_cleanup_due(true, last, DiskPressure::Pressure)
}

// The caller holds the maintenance lock. Missing telemetry changes accounting,
// not the ownership, process-use, age, and quarantine checks on each candidate.
fn run_unverified_cleanup(
    config: &GuardConfig,
    message: &str,
    last_pressure_cleanup: &mut Option<Instant>,
    last_cleanup_at: &mut Option<Timestamp>,
    cleanup_error: &mut Option<String>,
) {
    match run_cleanup(
        config,
        DiskPressure::Pressure,
        CleanupMode::Execute,
        None,
        CacheAgePolicy::Retention,
        Some(message),
    ) {
        Ok(cycle) if cycle.cleanup.succeeded() => {
            *last_cleanup_at = Some(Timestamp::now());
            *cleanup_error = cycle
                .cleanup
                .process_inspection_blocked()
                .then(|| "cleanup could not safely inspect a same-user process".to_owned())
                .or_else(|| cycle.trim_error.clone());
        }
        Ok(cycle) => {
            *cleanup_error = Some(format!(
                "cleanup finished with {} failures",
                cycle.cleanup.failures
            ));
        }
        Err(error) => *cleanup_error = Some(format!("cleanup failed: {error:#}")),
    }
    *last_pressure_cleanup = Some(Instant::now());
}

fn telemetry_failure_message(message: &str, cleanup_error: Option<&str>) -> String {
    cleanup_error.map_or_else(
        || message.to_owned(),
        |cleanup_error| format!("{message}; {cleanup_error}"),
    )
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
    use guardwsl::cleanup::{CleanupCandidate, CleanupKind, CleanupPlan};
    use guardwsl::config::ConfigOrigin;
    use guardwsl::host::HostPlatform;

    #[test]
    fn healthy_heartbeat_can_report_critical_pressure_without_safe_reclaim() {
        let now = Timestamp::now();
        let config = GuardConfig::default();
        let mut heartbeat: MonitorStatus = serde_json::from_value(json!({
            "schema_version": 1, "pid": std::process::id(), "started_at": now,
            "last_cycle_at": now, "next_maintenance_at": now, "last_cleanup_at": now,
            "last_pressure": "critical", "last_error": null
        }))
        .unwrap();
        assert!(!heartbeat.no_safe_reclaim);
        heartbeat.no_safe_reclaim = true;
        assert!(heartbeat.healthy(&config));
        heartbeat.last_error = Some("cleanup failed".to_owned());
        assert!(!heartbeat.healthy(&config));
        heartbeat.last_error = None;
        heartbeat.last_cycle_at = now - jiff::SignedDuration::from_hours(24);
        assert!(!heartbeat.healthy(&config));
    }

    #[test]
    fn default_cleanup_remains_enabled() {
        let config = GuardConfig::default();
        assert!(config.cleanup.enabled);
    }

    #[test]
    fn unavailable_host_uses_conservative_cleanup_without_inventing_capacity() {
        let mut config = GuardConfig::default();
        assert_eq!(cleanup_pressure(&config, None), DiskPressure::Pressure);
        assert!(unknown_host_cleanup_due(config.cleanup.enabled, None));
        assert!(!unknown_host_cleanup_due(
            config.cleanup.enabled,
            Some(Instant::now())
        ));
        let previous = Instant::now() - Duration::from_secs(121);
        assert!(unknown_host_cleanup_due(
            config.cleanup.enabled,
            Some(previous)
        ));
        config.cleanup.enabled = false;
        assert!(!unknown_host_cleanup_due(config.cleanup.enabled, None));
    }

    #[test]
    fn rising_pressure_shortens_the_current_cleanup_cooldown() {
        let previous = Instant::now() - Duration::from_secs(45);
        assert!(!pressure_cleanup_due(
            true,
            Some(previous),
            DiskPressure::Pressure
        ));
        assert!(!pressure_cleanup_due(
            true,
            Some(previous),
            DiskPressure::Critical
        ));
        assert!(pressure_cleanup_due(
            true,
            Some(previous),
            DiskPressure::Emergency
        ));
        assert!(!pressure_cleanup_due(
            false,
            Some(previous),
            DiskPressure::Emergency
        ));
        assert!(pressure_cleanup_due(true, None, DiskPressure::Pressure));
    }

    #[test]
    fn discard_reports_command_failure() {
        assert!(discard_guest_blocks_with("/bin/sh", &["-c", "exit 0"]).is_ok());
        assert!(discard_guest_blocks_with("/bin/sh", &["-c", "exit 3"]).is_err());
    }

    #[test]
    fn recent_caches_require_an_explicit_manual_flag() {
        let Command::Clean(default) = Cli::try_parse_from(["guard", "clean"]).unwrap().command
        else {
            panic!("expected clean")
        };
        assert!(!default.all_caches);
        let Command::Clean(explicit) =
            Cli::try_parse_from(["guard", "clean", "--all-caches", "--dry-run", "--json"])
                .unwrap()
                .command
        else {
            panic!("expected clean")
        };
        assert!(explicit.all_caches && explicit.dry_run && explicit.json);
        assert!(Cli::try_parse_from(["guard", "monitor", "--all-caches"]).is_err());
        assert!(Cli::try_parse_from(["guard", "status", "--all-caches"]).is_err());
    }

    #[test]
    fn forwarding_subcommands_are_not_available() {
        assert!(Cli::try_parse_from(["guard", "exec", "echo", "hello"]).is_err());
        assert!(Cli::try_parse_from(["guard", "shim", "npm"]).is_err());
    }

    #[test]
    fn config_origin_is_serializable() {
        assert_eq!(
            serde_json::to_value(ConfigOrigin::Default).unwrap(),
            json!("default")
        );
    }

    fn candidate(path: &str, estimated_bytes: u64) -> CleanupCandidate {
        CleanupCandidate {
            path: PathBuf::from(path),
            kind: CleanupKind::JavaScriptCache,
            repository: None,
            minimum_age_hours: 1,
            estimated_bytes,
            newest_modified_at: Timestamp::now(),
            device: 1,
            inode: 1,
        }
    }

    fn plan_with_sizes(sizes: &[u64]) -> CleanupPlan {
        CleanupPlan {
            created_at: Timestamp::now(),
            pressure: DiskPressure::Pressure,
            candidates: sizes
                .iter()
                .enumerate()
                .map(|(index, size)| candidate(&format!("/cache/{index}"), *size))
                .collect(),
            skips: Vec::new(),
        }
    }

    fn snapshot(sparse: bool, total: u64, free: u64) -> HostSnapshot {
        HostSnapshot {
            schema_version: 2,
            platform: HostPlatform::Wsl2,
            captured_at: Timestamp::now(),
            source: None,
            distro: Some("Ubuntu-24.04".to_owned()),
            vhdx_path: Some(r"X:\wsl\ext4.vhdx".to_owned()),
            vhdx_sparse: Some(sparse),
            vhdx_size_bytes: Some(total - free),
            volume_root: r"X:\".to_owned(),
            volume_total_bytes: total,
            volume_free_bytes: free,
        }
    }

    #[test]
    fn logical_budget_caps_reclaimable_cleanup() {
        let config = GuardConfig::default();
        let total = 100 * guardwsl::config::GIB;
        // 10 GiB free vs 64 GiB default target would budget 54 GiB; use a
        // smaller gap so the cap is visible.
        let mut tight = config.clone();
        tight.disk.target_free_bytes = 12 * guardwsl::config::GIB;
        let host = snapshot(true, total, 10 * guardwsl::config::GIB);
        let mut plan = plan_with_sizes(&[
            3 * guardwsl::config::GIB,
            3 * guardwsl::config::GIB,
            3 * guardwsl::config::GIB,
        ]);
        apply_logical_budget(&mut plan, &tight, &host, DiskPressure::Pressure, true, true);
        // Budget = 2 GiB; the first 3 GiB candidate is kept, the rest are not.
        assert_eq!(plan.candidates.len(), 1);
    }

    #[test]
    fn non_sparse_vhdx_skips_the_unreachable_host_budget() {
        let config = GuardConfig::default();
        let total = 100 * guardwsl::config::GIB;
        let mut tight = config.clone();
        tight.disk.target_free_bytes = 12 * guardwsl::config::GIB;
        let host = snapshot(false, total, 10 * guardwsl::config::GIB);
        let mut plan = plan_with_sizes(&[
            3 * guardwsl::config::GIB,
            3 * guardwsl::config::GIB,
            3 * guardwsl::config::GIB,
        ]);
        apply_logical_budget(
            &mut plan,
            &tight,
            &host,
            DiskPressure::Pressure,
            true,
            false,
        );
        assert_eq!(plan.candidates.len(), 3);
    }

    #[test]
    fn healthy_cycles_are_not_budget_capped() {
        let config = GuardConfig::default();
        let total = 100 * guardwsl::config::GIB;
        let host = snapshot(true, total, 10 * guardwsl::config::GIB);
        let mut plan = plan_with_sizes(&[
            3 * guardwsl::config::GIB,
            3 * guardwsl::config::GIB,
            3 * guardwsl::config::GIB,
        ]);
        apply_logical_budget(
            &mut plan,
            &config,
            &host,
            DiskPressure::Healthy,
            false,
            true,
        );
        assert_eq!(plan.candidates.len(), 3);
    }

    #[test]
    fn emergency_skips_logical_budget_even_on_sparse_vhdx() {
        let config = GuardConfig::default();
        let total = 100 * guardwsl::config::GIB;
        let mut tight = config.clone();
        tight.disk.target_free_bytes = 12 * guardwsl::config::GIB;
        let host = snapshot(true, total, 10 * guardwsl::config::GIB);
        let mut plan = plan_with_sizes(&[
            3 * guardwsl::config::GIB,
            3 * guardwsl::config::GIB,
            3 * guardwsl::config::GIB,
        ]);
        apply_logical_budget(
            &mut plan,
            &tight,
            &host,
            DiskPressure::Emergency,
            true,
            true,
        );
        assert_eq!(plan.candidates.len(), 3);
    }

    #[test]
    fn emergency_lifts_the_action_ceiling() {
        use guardwsl::cleanup::effective_max_actions;
        assert_eq!(effective_max_actions(DiskPressure::Healthy, 20), 20);
        assert_eq!(effective_max_actions(DiskPressure::Pressure, 20), 20);
        assert_eq!(effective_max_actions(DiskPressure::Critical, 20), 20);
        assert_eq!(effective_max_actions(DiskPressure::Emergency, 20), 40);
        assert_eq!(effective_max_actions(DiskPressure::Emergency, 60), 60);
        assert_eq!(effective_max_actions(DiskPressure::Emergency, 5), 40);
    }
}
