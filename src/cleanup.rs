//! Conservative cleanup for GuardWSL v1.
//!
//! Only exact names and contexts enter a plan. dist, build, out, databases,
//! Docker volumes, and unknown paths are never candidates.

use crate::config::GuardConfig;
use crate::fsutil::{effective_uid, is_unsafe_dir_owner_mode};
use crate::history::{AuditLog, AuditOutcome, AuditRecord};
use crate::host::DiskPressure;
use crate::repository::{Repository, discover_repositories};
use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::io::{self, Read};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
#[cfg(test)]
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};
use walkdir::{DirEntry, WalkDir};

const MAX_TREE_ENTRIES: usize = 2_000_000;
const MAX_DISCOVERED_PROJECTS: usize = 512;
const MAX_PROC_INSPECTION_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CACHED_PROCESS_BYTES: usize = 32 * 1024 * 1024;
const MAX_PLANNING_CANDIDATES: usize = 10_000;
pub const PROCESS_INSPECTION_ERROR_PREFIX: &str = "cannot inspect same-user process ";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupKind {
    JavaScriptCache,
    RustCache,
    GoCache,
    ToolCache,
    ProjectCache,
    RustTarget,
    NextBuild,
    NodeModules,
}

impl CleanupKind {
    const fn risk_rank(self) -> u8 {
        match self {
            Self::JavaScriptCache
            | Self::RustCache
            | Self::GoCache
            | Self::ToolCache
            | Self::ProjectCache => 0,
            Self::RustTarget | Self::NextBuild => 1,
            Self::NodeModules => 2,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JavaScriptCache => "javascript_cache",
            Self::RustCache => "rust_cache",
            Self::GoCache => "go_cache",
            Self::ToolCache => "tool_cache",
            Self::ProjectCache => "project_cache",
            Self::RustTarget => "rust_target",
            Self::NextBuild => "next_build",
            Self::NodeModules => "node_modules",
        }
    }

    /// Every cleanup kind in allowlist order (lowest risk first).
    pub const ALL: [Self; 8] = [
        Self::JavaScriptCache,
        Self::RustCache,
        Self::GoCache,
        Self::ToolCache,
        Self::ProjectCache,
        Self::RustTarget,
        Self::NextBuild,
        Self::NodeModules,
    ];

    /// Machine-readable allowlist names, kept in sync with `Self::ALL`.
    pub fn allowlist_names() -> Vec<&'static str> {
        Self::ALL.iter().map(|kind| kind.as_str()).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupCandidate {
    pub path: PathBuf,
    pub kind: CleanupKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<PathBuf>,
    pub minimum_age_hours: u64,
    #[serde(default)]
    pub early_toolchain_reclaim: bool,
    pub estimated_bytes: u64,
    pub newest_modified_at: Timestamp,
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupSkip {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupPlan {
    pub created_at: Timestamp,
    pub pressure: DiskPressure,
    pub candidates: Vec<CleanupCandidate>,
    pub skips: Vec<CleanupSkip>,
}

#[derive(Default)]
struct PlanningState {
    processes: PrivilegedProcessCache,
    seen: HashSet<(PathBuf, CleanupKind, Option<PathBuf>, u64)>,
    limit_reported: bool,
}

struct PrivilegedProcessCache {
    entries: HashMap<PathBuf, PrivilegedProcessSnapshot>,
    bytes: usize,
    max_bytes: usize,
}

impl Default for PrivilegedProcessCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            max_bytes: MAX_CACHED_PROCESS_BYTES,
        }
    }
}

struct PrivilegedProcessSnapshot {
    start_time: String,
    links: Vec<u8>,
    maps: Vec<u8>,
    command_line: Vec<u8>,
}

impl PrivilegedProcessSnapshot {
    fn bytes(&self) -> usize {
        self.links.len() + self.maps.len() + self.command_line.len()
    }
}

#[cfg(target_os = "linux")]
impl PrivilegedProcessCache {
    fn references_or_collect<F>(
        &mut self,
        process: &Path,
        start_time: &str,
        candidate: &Path,
        repository: Option<&Path>,
        kind: CleanupKind,
        collect: F,
    ) -> Result<bool>
    where
        F: FnOnce() -> Result<PrivilegedProcessSnapshot>,
    {
        if let Some(snapshot) = self.entries.get(process)
            && snapshot.start_time == start_time
        {
            return snapshot_references(snapshot, candidate, repository, kind);
        }
        if let Some(previous) = self.entries.remove(process) {
            self.bytes = self.bytes.saturating_sub(previous.bytes());
        }
        let snapshot = collect()?;
        if snapshot.start_time != start_time {
            bail!("process identity changed during inspection")
        }
        let referenced = snapshot_references(&snapshot, candidate, repository, kind)?;
        if self.bytes.saturating_add(snapshot.bytes()) <= self.max_bytes {
            self.bytes += snapshot.bytes();
            self.entries.insert(process.to_path_buf(), snapshot);
        }
        Ok(referenced)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupMode {
    DryRun,
    Execute,
}

/// Explicit manual cache retention policy; automatic maintenance keeps retention.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheAgePolicy {
    #[default]
    Retention,
    IncludeRecent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupActionOutcome {
    WouldRemove,
    Removed,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupAction {
    pub path: PathBuf,
    pub kind: CleanupKind,
    pub outcome: CleanupActionOutcome,
    pub logical_bytes: u64,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupReport {
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
    pub mode: CleanupMode,
    pub pressure: DiskPressure,
    pub planned_logical_bytes: u64,
    pub deleted_logical_bytes: u64,
    pub actions: Vec<CleanupAction>,
    pub planning_skips: Vec<CleanupSkip>,
    pub failures: usize,
}

impl CleanupReport {
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.failures == 0
    }

    #[must_use]
    pub fn process_inspection_blocked(&self) -> bool {
        self.planning_skips
            .iter()
            .any(|skip| skip.reason.starts_with(PROCESS_INSPECTION_ERROR_PREFIX))
            || self
                .actions
                .iter()
                .any(|action| action.detail.starts_with(PROCESS_INSPECTION_ERROR_PREFIX))
    }
}

pub fn plan_cleanup(
    config: &GuardConfig,
    pressure: DiskPressure,
    need_space: bool,
) -> Result<CleanupPlan> {
    plan_cleanup_with_cache_policy(config, pressure, need_space, CacheAgePolicy::Retention)
}

pub fn plan_cleanup_with_cache_policy(
    config: &GuardConfig,
    pressure: DiskPressure,
    need_space: bool,
    cache_policy: CacheAgePolicy,
) -> Result<CleanupPlan> {
    let mut plan = CleanupPlan {
        created_at: Timestamp::now(),
        pressure,
        candidates: Vec::new(),
        skips: Vec::new(),
    };
    if !config.cleanup.enabled {
        return Ok(plan);
    }

    let compress_ages = need_space || pressure != DiskPressure::Healthy;
    let mut state = PlanningState::default();
    add_global_caches(config, compress_ages, cache_policy, &mut plan, &mut state);
    let discovery = discover_repositories(&config.cleanup.scan_roots)?;
    plan.skips
        .extend(discovery.skipped.into_iter().map(|skip| CleanupSkip {
            path: skip.path,
            reason: format!("repository_discovery: {}", skip.reason),
        }));
    for repository in discovery.repositories {
        if is_guard_quarantine_path(&repository.root) {
            plan.skips.push(CleanupSkip {
                path: repository.root,
                reason: "repository is inside a preserved quarantine".to_owned(),
            });
            continue;
        }
        add_repository_candidates(
            config,
            compress_ages,
            cache_policy,
            &repository,
            &mut plan,
            &mut state,
        );
    }

    plan.candidates.sort_by(|left, right| {
        left.kind
            .risk_rank()
            .cmp(&right.kind.risk_rank())
            .then(left.newest_modified_at.cmp(&right.newest_modified_at))
            .then(right.estimated_bytes.cmp(&left.estimated_bytes))
            .then(left.path.cmp(&right.path))
    });
    plan.candidates
        .dedup_by(|left, right| left.path == right.path);
    Ok(plan)
}

/// Action ceiling for one cleanup cycle.
///
/// `Emergency` raises the ceiling so regenerable artifacts are cleared before
/// anything outside GuardWSL's scope (like an offline VHDX compact) is needed.
/// Risk order still applies: caches first, then build outputs, then
/// `node_modules`.
#[must_use]
pub fn effective_max_actions(pressure: DiskPressure, configured: usize) -> usize {
    if pressure == DiskPressure::Emergency {
        configured.max(40)
    } else {
        configured
    }
}

pub fn execute_cleanup(
    config: &GuardConfig,
    plan: &CleanupPlan,
    mode: CleanupMode,
    audit: &AuditLog,
) -> Result<CleanupReport> {
    execute_cleanup_with_hook(config, plan, mode, audit, |_| {})
}

fn execute_cleanup_with_hook<F>(
    config: &GuardConfig,
    plan: &CleanupPlan,
    mode: CleanupMode,
    audit: &AuditLog,
    mut before_final_process_check: F,
) -> Result<CleanupReport>
where
    F: FnMut(&CleanupCandidate),
{
    let started_at = Timestamp::now();
    let mut report = CleanupReport {
        started_at,
        finished_at: started_at,
        mode,
        pressure: plan.pressure,
        planned_logical_bytes: plan
            .candidates
            .iter()
            .map(|candidate| candidate.estimated_bytes)
            .sum(),
        deleted_logical_bytes: 0,
        actions: Vec::new(),
        planning_skips: plan.skips.clone(),
        failures: 0,
    };

    let max_actions = effective_max_actions(plan.pressure, config.cleanup.max_actions_per_cycle);
    for planned in plan.candidates.iter().take(max_actions) {
        if mode == CleanupMode::DryRun {
            audit.append(
                &AuditRecord::new(
                    "cleanup",
                    AuditOutcome::Planned,
                    format!(
                        "dry run: logical {} removal; minimum_age_hours={}; early_toolchain_reclaim={}; no mutation performed",
                        planned.kind.as_str(),
                        planned.minimum_age_hours,
                        planned.early_toolchain_reclaim
                    ),
                )
                .for_path(&planned.path)
                .with_estimated_bytes(planned.estimated_bytes),
            )?;
            report.actions.push(CleanupAction {
                path: planned.path.clone(),
                kind: planned.kind,
                outcome: CleanupActionOutcome::WouldRemove,
                logical_bytes: planned.estimated_bytes,
                detail: "candidate revalidated during planning; no mutation performed".to_owned(),
            });
            continue;
        }

        let current = match inspect_candidate_with_cache(
            config,
            &planned.path,
            planned.kind,
            planned.repository.as_deref(),
            planned.minimum_age_hours,
            plan.pressure,
            &mut PrivilegedProcessCache::default(),
        ) {
            Ok(candidate)
                if candidate.device == planned.device
                    && candidate.inode == planned.inode
                    && candidate.newest_modified_at == planned.newest_modified_at =>
            {
                candidate
            }
            Ok(_) => {
                audit.append(
                    &AuditRecord::new(
                        "cleanup",
                        AuditOutcome::Skipped,
                        "candidate changed after planning",
                    )
                    .for_path(&planned.path),
                )?;
                report.actions.push(CleanupAction {
                    path: planned.path.clone(),
                    kind: planned.kind,
                    outcome: CleanupActionOutcome::Skipped,
                    logical_bytes: 0,
                    detail: "inode or mtime changed after planning".to_owned(),
                });
                continue;
            }
            Err(error) => {
                audit.append(
                    &AuditRecord::new("cleanup", AuditOutcome::Skipped, error.to_string())
                        .for_path(&planned.path),
                )?;
                report.actions.push(CleanupAction {
                    path: planned.path.clone(),
                    kind: planned.kind,
                    outcome: CleanupActionOutcome::Skipped,
                    logical_bytes: 0,
                    detail: error.to_string(),
                });
                continue;
            }
        };

        audit.append(
            &AuditRecord::new(
                "cleanup",
                AuditOutcome::Planned,
                format!(
                    "logical {} removal; minimum_age_hours={}; early_toolchain_reclaim={}",
                    current.kind.as_str(),
                    current.minimum_age_hours,
                    current.early_toolchain_reclaim
                ),
            )
            .for_path(&current.path)
            .with_estimated_bytes(current.estimated_bytes),
        )?;

        before_final_process_check(&current);
        // Never reuse the planning snapshot for a real removal. A process can
        // open the candidate while its tree is profiled or the audit is written.
        let process_check =
            path_is_in_use(&current.path, current.repository.as_deref(), current.kind);
        let skip = match process_check {
            Ok(false) => None,
            Ok(true) => Some("candidate is in use immediately before removal".to_owned()),
            Err(error) => Some(format!("{error:#}")),
        };
        if let Some(reason) = skip {
            audit.append(
                &AuditRecord::new("cleanup", AuditOutcome::Skipped, &reason)
                    .for_path(&current.path),
            )?;
            report.actions.push(CleanupAction {
                path: current.path,
                kind: current.kind,
                outcome: CleanupActionOutcome::Skipped,
                logical_bytes: 0,
                detail: reason,
            });
            continue;
        }

        match quarantine_and_remove(&current.path, current.device, current.inode) {
            Ok(()) => {
                audit.append(
                    &AuditRecord::new("cleanup", AuditOutcome::Success, "removal completed")
                        .for_path(&current.path)
                        .with_estimated_bytes(current.estimated_bytes),
                )?;
                report.deleted_logical_bytes = report
                    .deleted_logical_bytes
                    .saturating_add(current.estimated_bytes);
                report.actions.push(CleanupAction {
                    path: current.path,
                    kind: current.kind,
                    outcome: CleanupActionOutcome::Removed,
                    logical_bytes: current.estimated_bytes,
                    detail: "directory renamed on the same mount and removed".to_owned(),
                });
            }
            Err(error) => {
                report.failures += 1;
                audit.append(
                    &AuditRecord::new("cleanup", AuditOutcome::Failed, error.to_string())
                        .for_path(&current.path)
                        .with_estimated_bytes(current.estimated_bytes),
                )?;
                report.actions.push(CleanupAction {
                    path: current.path,
                    kind: current.kind,
                    outcome: CleanupActionOutcome::Failed,
                    logical_bytes: 0,
                    detail: error.to_string(),
                });
            }
        }
    }
    report.finished_at = Timestamp::now();
    Ok(report)
}

fn add_global_caches(
    config: &GuardConfig,
    compress_ages: bool,
    cache_policy: CacheAgePolicy,
    plan: &mut CleanupPlan,
    state: &mut PlanningState,
) {
    let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    else {
        plan.skips.push(CleanupSkip {
            path: PathBuf::from("$HOME"),
            reason: "an absolute HOME is unavailable".to_owned(),
        });
        return;
    };
    let age = cache_age_for(config, compress_ages, cache_policy);
    for (path, kind) in [
        (home.join(".npm/_cacache"), CleanupKind::JavaScriptCache),
        (home.join(".cache/yarn"), CleanupKind::JavaScriptCache),
        (home.join(".cache/pnpm"), CleanupKind::JavaScriptCache),
        (home.join(".yarn/berry/cache"), CleanupKind::JavaScriptCache),
        (
            home.join(".bun/install/cache"),
            CleanupKind::JavaScriptCache,
        ),
        (
            home.join(".local/share/pnpm/store"),
            CleanupKind::JavaScriptCache,
        ),
        (home.join(".cargo/registry/cache"), CleanupKind::RustCache),
        (home.join(".cargo/git/checkouts"), CleanupKind::RustCache),
        (home.join(".cache/go-build"), CleanupKind::GoCache),
        (home.join("go/pkg/mod/cache"), CleanupKind::GoCache),
        (home.join(".cache/sccache"), CleanupKind::ToolCache),
        (home.join(".cache/vscode-cpptools"), CleanupKind::ToolCache),
        (home.join(".cache/ms-playwright"), CleanupKind::ToolCache),
    ] {
        push_candidate(config, path, kind, None, age, plan, state);
    }
}

fn add_repository_candidates(
    config: &GuardConfig,
    compress_ages: bool,
    cache_policy: CacheAgePolicy,
    repository: &Repository,
    plan: &mut CleanupPlan,
    state: &mut PlanningState,
) {
    let cache_age = cache_age_for(config, compress_ages, cache_policy);
    for name in [
        ".turbo",
        ".vite",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
    ] {
        push_candidate(
            config,
            repository.root.join(name),
            CleanupKind::ProjectCache,
            Some(&repository.root),
            cache_age,
            plan,
            state,
        );
    }

    let project_roots = discover_project_roots(&repository.root);
    for project in project_roots.into_iter().take(MAX_DISCOVERED_PROJECTS) {
        if project.join("Cargo.toml").is_file() {
            push_candidate(
                config,
                project.join("target"),
                CleanupKind::RustTarget,
                Some(&repository.root),
                age_for(config, compress_ages, config.cleanup.build_min_age_hours),
                plan,
                state,
            );
        }
        if project.join("package.json").is_file() {
            push_candidate(
                config,
                project.join(".next"),
                CleanupKind::NextBuild,
                Some(&repository.root),
                age_for(config, compress_ages, config.cleanup.build_min_age_hours),
                plan,
                state,
            );
            if find_lockfile(&project, Some(&repository.root)) {
                push_candidate(
                    config,
                    project.join("node_modules"),
                    CleanupKind::NodeModules,
                    Some(&repository.root),
                    age_for(
                        config,
                        compress_ages,
                        config.cleanup.node_modules_min_age_hours,
                    ),
                    plan,
                    state,
                );
            }
        }
    }
}

fn discover_project_roots(repository: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let walker = WalkDir::new(repository)
        .follow_links(false)
        .max_depth(4)
        .into_iter()
        .filter_entry(project_walk_entry);
    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        if matches!(
            entry.file_name().to_str(),
            Some("package.json" | "Cargo.toml")
        ) && let Some(parent) = entry.path().parent()
        {
            roots.push(parent.to_path_buf());
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn project_walk_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    if entry.file_type().is_dir() && !has_no_git_marker(entry.path()) {
        return false;
    }
    let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
    if name.starts_with(".guardwsl-trash-") {
        return false;
    }
    !matches!(
        name.as_str(),
        ".git"
            | "node_modules"
            | "target"
            | ".next"
            | ".turbo"
            | ".cache"
            | "dist"
            | "build"
            | "out"
            | "vendor"
            | "backups"
            | "backup"
    )
}

fn is_guard_quarantine_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .starts_with(".guardwsl-trash-")
    })
}

fn dir_has_lockfile(dir: &Path) -> bool {
    [
        "package-lock.json",
        "npm-shrinkwrap.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lock",
        "bun.lockb",
    ]
    .iter()
    .any(|name| regular_file(&dir.join(name)))
}

fn find_lockfile(project: &Path, repository: Option<&Path>) -> bool {
    if dir_has_lockfile(project) {
        return true;
    }
    let Some(repo) = repository else {
        return false;
    };
    let mut dir = project.parent();
    while let Some(current) = dir {
        if current == repo || !current.starts_with(repo) {
            break;
        }
        if dir_has_lockfile(current) {
            return true;
        }
        dir = current.parent();
    }
    dir_has_lockfile(repo)
}

fn age_for(config: &GuardConfig, compress_ages: bool, normal: u64) -> u64 {
    if compress_ages {
        config.cleanup.critical_min_age_hours.min(normal)
    } else {
        normal
    }
}

fn cache_age_for(config: &GuardConfig, compress_ages: bool, policy: CacheAgePolicy) -> u64 {
    match policy {
        CacheAgePolicy::Retention => {
            age_for(config, compress_ages, config.cleanup.cache_min_age_hours)
        }
        CacheAgePolicy::IncludeRecent => 0,
    }
}

fn push_candidate(
    config: &GuardConfig,
    path: PathBuf,
    kind: CleanupKind,
    repository: Option<&Path>,
    minimum_age_hours: u64,
    plan: &mut CleanupPlan,
    state: &mut PlanningState,
) {
    if !path.exists() {
        return;
    }
    let key = (
        path.clone(),
        kind,
        repository.map(Path::to_path_buf),
        minimum_age_hours,
    );
    if state.seen.contains(&key) {
        return;
    }
    if state.seen.len() >= MAX_PLANNING_CANDIDATES {
        if !state.limit_reported {
            plan.skips.push(CleanupSkip {
                path,
                reason: "candidate planning limit reached; remaining paths preserved".to_owned(),
            });
            state.limit_reported = true;
        }
        return;
    }
    state.seen.insert(key);
    match inspect_candidate_with_cache(
        config,
        &path,
        kind,
        repository,
        minimum_age_hours,
        plan.pressure,
        &mut state.processes,
    ) {
        Ok(candidate) => plan.candidates.push(candidate),
        Err(error) => plan.skips.push(CleanupSkip {
            path,
            reason: error.to_string(),
        }),
    }
}

#[cfg(test)]
fn inspect_candidate(
    config: &GuardConfig,
    path: &Path,
    kind: CleanupKind,
    repository: Option<&Path>,
    minimum_age_hours: u64,
) -> Result<CleanupCandidate> {
    inspect_candidate_with_cache(
        config,
        path,
        kind,
        repository,
        minimum_age_hours,
        DiskPressure::Healthy,
        &mut PrivilegedProcessCache::default(),
    )
}

fn inspect_candidate_with_cache(
    config: &GuardConfig,
    path: &Path,
    kind: CleanupKind,
    repository: Option<&Path>,
    minimum_age_hours: u64,
    pressure: DiskPressure,
    process_cache: &mut PrivilegedProcessCache,
) -> Result<CleanupCandidate> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("candidate is not a real directory")
    }
    #[cfg(target_os = "linux")]
    if is_unsafe_dir_owner_mode(metadata.uid(), metadata.mode()) {
        bail!("candidate is not a private root owned by the current user")
    }
    let parent = path.parent().context("candidate has no parent directory")?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    #[cfg(target_os = "linux")]
    if parent_metadata.file_type().is_symlink()
        || is_unsafe_dir_owner_mode(parent_metadata.uid(), parent_metadata.mode())
    {
        bail!("candidate parent directory has an unsafe owner or mode")
    }
    let canonical = fs::canonicalize(path)?;
    if canonical != path {
        bail!("candidate is not canonical")
    }
    for protected in &config.cleanup.protected_paths {
        let protected = normalize_protected_path(protected)?;
        if canonical.starts_with(&protected) || protected.starts_with(&canonical) {
            bail!(
                "candidate intersects protected path {}",
                protected.display()
            )
        }
    }
    validate_context(&canonical, kind, repository)?;
    if let Some(repository) = repository {
        validate_git_state(repository, &canonical)?;
    }
    if path_is_in_use_with_cache(&canonical, repository, kind, process_cache)? {
        bail!("candidate is in use by an active process")
    }
    let profile = profile_tree(&canonical)?;
    let age = SystemTime::now()
        .duration_since(profile.newest_modified)
        .context("candidate mtime is in the future")?;
    let too_recent = age < Duration::from_secs(minimum_age_hours.saturating_mul(3600));
    let early_toolchain_reclaim = too_recent
        && qualifies_for_early_toolchain_reclaim(
            pressure,
            kind,
            &canonical,
            profile.allocated_bytes,
            config.cleanup.critical_toolchain_min_bytes,
        );
    if too_recent && !early_toolchain_reclaim {
        bail!(
            "candidate is {}h old; minimum age is {}h",
            age.as_secs() / 3600,
            minimum_age_hours
        )
    }
    Ok(CleanupCandidate {
        path: canonical,
        kind,
        repository: repository.map(Path::to_path_buf),
        minimum_age_hours,
        early_toolchain_reclaim,
        estimated_bytes: profile.bytes,
        newest_modified_at: Timestamp::try_from(profile.newest_modified)?,
        device: profile.device,
        inode: profile.inode,
    })
}

fn qualifies_for_early_toolchain_reclaim(
    pressure: DiskPressure,
    kind: CleanupKind,
    path: &Path,
    allocated_bytes: u64,
    minimum_bytes: u64,
) -> bool {
    if minimum_bytes == 0
        || allocated_bytes < minimum_bytes
        || !matches!(pressure, DiskPressure::Critical | DiskPressure::Emergency)
    {
        return false;
    }
    matches!(
        kind,
        CleanupKind::GoCache | CleanupKind::RustCache | CleanupKind::RustTarget
    ) || (kind == CleanupKind::ToolCache && path.ends_with(".cache/sccache"))
}

fn normalize_protected_path(path: &Path) -> Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(canonical) => Ok(canonical),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            normalize_absolute_path(path)
        }
        Err(error) => Err(error)
            .with_context(|| format!("could not normalize protected path {}", path.display())),
    }
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("protected path is not absolute: {}", path.display())
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("protected path escapes the root: {}", path.display())
                }
            }
        }
    }
    Ok(normalized)
}

fn validate_context(path: &Path, kind: CleanupKind, repository: Option<&Path>) -> Result<()> {
    match kind {
        CleanupKind::RustTarget => {
            let parent = path.parent().context("target has no parent directory")?;
            if !regular_file(&parent.join("Cargo.toml")) {
                bail!("target has no sibling Cargo.toml")
            }
        }
        CleanupKind::NextBuild => {
            let parent = path.parent().context(".next has no parent directory")?;
            if !regular_file(&parent.join("package.json")) {
                bail!(".next has no sibling package.json")
            }
            if !find_lockfile(parent, repository) {
                bail!(".next has no lockfile in the project directory or repository root")
            }
        }
        CleanupKind::NodeModules => {
            let parent = path
                .parent()
                .context("node_modules has no parent directory")?;
            if !regular_file(&parent.join("package.json")) {
                bail!("node_modules has no sibling package.json")
            }
            if !find_lockfile(parent, repository) {
                bail!("node_modules has no lockfile in the project directory or repository root")
            }
        }
        CleanupKind::ProjectCache => {
            if repository.is_none() {
                bail!("project cache has no Git identity")
            }
        }
        CleanupKind::JavaScriptCache
        | CleanupKind::RustCache
        | CleanupKind::GoCache
        | CleanupKind::ToolCache => {}
    }
    Ok(())
}

fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

fn has_no_git_marker(directory: &Path) -> bool {
    matches!(fs::symlink_metadata(directory.join(".git")), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

fn validate_git_state(repository: &Path, candidate: &Path) -> Result<()> {
    let relative = candidate
        .strip_prefix(repository)
        .context("candidate escaped the repository")?;
    for directory in candidate.ancestors().take_while(|path| *path != repository) {
        if !has_no_git_marker(directory) {
            bail!("candidate crosses a Git repository boundary")
        }
    }
    let relative = relative.to_string_lossy();
    let literal = format!(":(literal){relative}");
    let tracked = git_status(
        repository,
        ["ls-files", "--error-unmatch", "--", literal.as_str()],
    )?;
    if tracked == 0 {
        bail!("candidate contains a Git-tracked path")
    }
    if tracked != 1 {
        bail!("git ls-files failed with status {tracked}")
    }
    let ignored = git_status(repository, ["check-ignore", "-q", "--", relative.as_ref()])?;
    if ignored != 0 {
        bail!("candidate is not ignored by Git")
    }
    Ok(())
}

fn git_status<const N: usize>(repository: &Path, args: [&str; N]) -> Result<i32> {
    let mut git_args = vec!["-c".into(), "safe.directory=*".into(), "-C".into()];
    git_args.push(repository.as_os_str().to_owned());
    git_args.extend(args.into_iter().map(Into::into));
    let output = crate::host::run_bounded_cleanup_command(
        "git",
        git_args,
        Duration::from_secs(3),
        64 * 1024,
    )
    .context("git is unavailable or did not complete")?;
    if output.stdout_truncated || output.stderr_truncated {
        bail!("git output exceeded limit")
    }
    Ok(output.status.code().unwrap_or(128))
}

#[derive(Debug)]
struct TreeProfile {
    bytes: u64,
    allocated_bytes: u64,
    newest_modified: SystemTime,
    device: u64,
    inode: u64,
}

struct HardLinkTally {
    nlink: u64,
    names_inside: usize,
    sample: PathBuf,
}

fn profile_tree(path: &Path) -> Result<TreeProfile> {
    let root = fs::symlink_metadata(path)?;
    #[cfg(target_os = "linux")]
    let (device, inode) = (root.dev(), root.ino());
    #[cfg(not(target_os = "linux"))]
    let (device, inode) = (0, 0);
    let mut bytes = 0_u64;
    let mut allocated_bytes = 0_u64;
    let mut newest = root.modified()?;
    let mounts = mount_points()?;
    let mut hard_links: HashMap<(u64, u64), HardLinkTally> = HashMap::new();
    for (index, entry) in WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .enumerate()
    {
        if index >= MAX_TREE_ENTRIES {
            bail!("candidate exceeds {MAX_TREE_ENTRIES} entries")
        }
        let entry = entry?;
        let entry_path = entry.path();
        if entry.file_name() == ".git" {
            bail!("candidate contains Git data")
        }
        if mounts.iter().any(|mount| mount == entry_path) {
            bail!(
                "candidate contains a nested mount at {}",
                entry_path.display()
            )
        }
        let metadata = fs::symlink_metadata(entry_path)?;
        #[cfg(target_os = "linux")]
        if metadata.dev() != device {
            bail!("candidate crosses a filesystem at {}", entry_path.display())
        }
        #[cfg(target_os = "linux")]
        {
            let mut first_name = true;
            if metadata.is_file() && metadata.nlink() > 1 {
                let key = (metadata.dev(), metadata.ino());
                let tally = hard_links.entry(key).or_insert_with(|| HardLinkTally {
                    nlink: metadata.nlink(),
                    names_inside: 0,
                    sample: entry_path.to_path_buf(),
                });
                tally.nlink = tally.nlink.max(metadata.nlink());
                tally.names_inside += 1;
                first_name = tally.names_inside == 1;
            }
            if first_name {
                allocated_bytes =
                    allocated_bytes.saturating_add(metadata.blocks().saturating_mul(512));
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            allocated_bytes = allocated_bytes.saturating_add(metadata.len());
        }
        if !metadata.is_dir() && !metadata.is_file() && !metadata.file_type().is_symlink() {
            bail!(
                "candidate contains a special file type at {}",
                entry_path.display()
            )
        }
        bytes = bytes.saturating_add(metadata.len());
        if let Ok(modified) = metadata.modified() {
            newest = newest.max(modified);
        }
    }
    for tally in hard_links.values() {
        if tally.names_inside < tally.nlink as usize {
            bail!(
                "candidate contains a hard link with a name outside the tree at {}",
                tally.sample.display()
            )
        }
    }
    Ok(TreeProfile {
        bytes,
        allocated_bytes,
        newest_modified: newest,
        device,
        inode,
    })
}

fn mount_points() -> Result<Vec<PathBuf>> {
    let text = fs::read_to_string("/proc/self/mountinfo")?;
    Ok(text
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .map(unescape_mount_path)
        .map(PathBuf::from)
        .collect())
}

fn unescape_mount_path(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// Inspects only processes with the same EUID as GuardWSL.
///
/// This boundary is intentional: candidates, their parents, and scan roots are
/// authenticated as owner-only writable through their immediate parent.
/// An inaccessible process is checked through a bounded, read-only privileged
/// fallback when available. Uncertainty always preserves the candidate.
fn path_is_in_use(candidate: &Path, repository: Option<&Path>, kind: CleanupKind) -> Result<bool> {
    path_is_in_use_with_cache(
        candidate,
        repository,
        kind,
        &mut PrivilegedProcessCache::default(),
    )
}

fn path_is_in_use_with_cache(
    candidate: &Path,
    repository: Option<&Path>,
    kind: CleanupKind,
    process_cache: &mut PrivilegedProcessCache,
) -> Result<bool> {
    let proc = fs::read_dir("/proc").context("cannot inspect same-user process list")?;
    let current_uid = effective_uid();
    let current_pid = std::process::id().to_string();
    for process in proc {
        let process = process.context("cannot inspect same-user process list")?;
        let pid_str = process.file_name();
        let pid_bytes = pid_str.to_string_lossy();
        if !pid_bytes.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let is_self = pid_bytes == current_pid.as_str();
        let metadata = match process.metadata() {
            Ok(metadata) => metadata,
            Err(error) if proc_entry_disappeared(&error) => continue,
            Err(error) => {
                bail!("{PROCESS_INSPECTION_ERROR_PREFIX}{}: {error}", pid_bytes)
            }
        };
        #[cfg(target_os = "linux")]
        if metadata.uid() != current_uid {
            continue;
        }
        let mut inspection_denied = false;
        for link in ["cwd", "root", "exe"] {
            match fs::read_link(process.path().join(link)) {
                Ok(path) if reference_hits(&path, candidate) => return Ok(true),
                Ok(path)
                    if !is_self
                        && link == "cwd"
                        && repository.is_some_and(|root| reference_hits(&path, root)) =>
                {
                    return Ok(true);
                }
                Ok(_) => {}
                Err(error) if proc_entry_disappeared(&error) => {}
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied && !is_self => {
                    inspection_denied = true;
                }
                Err(_) if !is_self => inspection_denied = true,
                Err(error) => {
                    bail!("{PROCESS_INSPECTION_ERROR_PREFIX}{}: {error}", pid_bytes)
                }
            }
        }
        let fd_dir = process.path().join("fd");
        match fs::read_dir(fd_dir) {
            Ok(entries) => {
                for fd in entries {
                    let Ok(fd) = fd else {
                        if !is_self {
                            inspection_denied = true;
                        }
                        continue;
                    };
                    match fs::read_link(fd.path()) {
                        Ok(path) if reference_hits(&path, candidate) => return Ok(true),
                        Ok(_) => {}
                        Err(error) if proc_entry_disappeared(&error) => {}
                        Err(error)
                            if error.kind() == io::ErrorKind::PermissionDenied && !is_self =>
                        {
                            inspection_denied = true;
                        }
                        Err(_) if !is_self => inspection_denied = true,
                        Err(error) => {
                            bail!("{PROCESS_INSPECTION_ERROR_PREFIX}{}: {error}", pid_bytes)
                        }
                    }
                }
            }
            Err(error) if proc_entry_disappeared(&error) => {}
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied && !is_self => {
                inspection_denied = true;
            }
            Err(_) if !is_self => inspection_denied = true,
            Err(error) => {
                bail!("{PROCESS_INSPECTION_ERROR_PREFIX}{}: {error}", pid_bytes)
            }
        }
        match read_proc_file_limited(&process.path().join("maps")) {
            Ok(maps) => {
                if mapped_path_hits(&maps, candidate)
                    .with_context(|| format!("{PROCESS_INSPECTION_ERROR_PREFIX}{}", pid_bytes))?
                {
                    return Ok(true);
                }
            }
            Err(error) if proc_entry_disappeared(&error) => continue,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied && !is_self => {
                inspection_denied = true;
            }
            Err(error) => {
                bail!("{PROCESS_INSPECTION_ERROR_PREFIX}{}: {error}", pid_bytes)
            }
        }
        let command_line = match fs::read(process.path().join("cmdline")) {
            Ok(command_line) => command_line,
            Err(error) if proc_entry_disappeared(&error) => continue,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied && !is_self => {
                inspection_denied = true;
                Vec::new()
            }
            Err(error) => {
                bail!("{PROCESS_INSPECTION_ERROR_PREFIX}{}: {error}", pid_bytes)
            }
        };
        if command_line_hits(&command_line, candidate) {
            return Ok(true);
        }
        if !is_self && repository.is_some_and(|root| command_line_hits(&command_line, root)) {
            return Ok(true);
        }
        if !is_self
            && active_tool_may_use_cache(kind, &command_line)
            && !go_uses_separate_build_cache(&process.path(), candidate, kind)
        {
            return Ok(true);
        }
        if inspection_denied {
            if command_line.is_empty() && process_is_zombie(&process.path()) {
                continue;
            }
            match privileged_process_references(
                &process.path(),
                current_uid,
                candidate,
                repository,
                kind,
                process_cache,
            ) {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(_)
                    if fs::metadata(process.path()).is_err_and(|e| proc_entry_disappeared(&e))
                        || process_is_zombie(&process.path()) =>
                {
                    continue;
                }
                Err(error) => {
                    bail!("{PROCESS_INSPECTION_ERROR_PREFIX}{}: {error:#}", pid_bytes)
                }
            }
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn command_line_hits(command_line: &[u8], candidate: &Path) -> bool {
    command_line.split(|byte| *byte == 0).any(|argument| {
        let value = argument
            .iter()
            .position(|byte| *byte == b'=')
            .map_or(argument, |index| &argument[index + 1..]);
        value.starts_with(b"/")
            && reference_hits(Path::new(std::ffi::OsStr::from_bytes(value)), candidate)
    })
}

#[cfg(not(target_os = "linux"))]
fn command_line_hits(_command_line: &[u8], _candidate: &Path) -> bool {
    false
}

fn active_tool_may_use_cache(kind: CleanupKind, command_line: &[u8]) -> bool {
    let names: &[&[u8]] = match kind {
        CleanupKind::GoCache => &[b"go", b"gopls"],
        CleanupKind::RustCache => &[b"cargo"],
        CleanupKind::JavaScriptCache => &[
            b"npm",
            b"npx",
            b"pnpm",
            b"yarn",
            b"bun",
            b"npm-cli.js",
            b"npx-cli.js",
            b"pnpm.cjs",
            b"yarn.js",
        ],
        CleanupKind::ToolCache => &[b"sccache"],
        _ => return false,
    };
    command_line.split(|byte| *byte == 0).any(|argument| {
        let name = argument
            .rsplit(|byte| *byte == b'/')
            .next()
            .unwrap_or_default();
        names.contains(&name)
    })
}

#[cfg(target_os = "linux")]
fn go_uses_separate_build_cache(process: &Path, candidate: &Path, kind: CleanupKind) -> bool {
    if kind != CleanupKind::GoCache || !candidate.ends_with(".cache/go-build") {
        return false;
    }
    if fs::read_link(process.join("exe"))
        .ok()
        .as_deref()
        .and_then(Path::file_name)
        != Some(std::ffi::OsStr::new("go"))
    {
        return false;
    }
    // Only interpret absolute paths when the process sees the same mounts and root.
    for part in ["ns/mnt", "root"] {
        let (Ok(theirs), Ok(ours)) = (
            fs::metadata(process.join(part)),
            fs::metadata(Path::new("/proc/self").join(part)),
        ) else {
            return false;
        };
        if (theirs.dev(), theirs.ino()) != (ours.dev(), ours.ino()) {
            return false;
        }
    }
    // An unreadable environment or an external cache program leaves the default protected.
    let Ok(env) = fs::read(process.join("environ")) else {
        return false;
    };
    let mut entries = env.split(|b| *b == 0);
    if entries.clone().any(|entry| {
        entry
            .strip_prefix(b"GOCACHEPROG=")
            .is_some_and(|v| !v.is_empty())
    }) {
        return false;
    }
    let Some(raw) = entries.find_map(|entry| entry.strip_prefix(b"GOCACHE=")) else {
        return false;
    };
    let configured = Path::new(std::ffi::OsStr::from_bytes(raw));
    if !configured.is_absolute() {
        return false;
    }
    let Ok(resolved) = configured.canonicalize() else {
        return false;
    };
    if !resolved.is_dir() || candidate.starts_with(&resolved) || resolved.starts_with(candidate) {
        return false;
    }
    // A different mount on the same device may bind a subtree of the default.
    let Ok(mounts) = mount_points() else {
        return false;
    };
    let containing_mount = |path: &Path| {
        mounts
            .iter()
            .filter(|mount| path.starts_with(mount))
            .max_by_key(|mount| mount.components().count())
    };
    let Some(default_mount) = containing_mount(candidate) else {
        return false;
    };
    containing_mount(&resolved) == Some(default_mount)
}

#[cfg(not(target_os = "linux"))]
fn go_uses_separate_build_cache(_process: &Path, _candidate: &Path, _kind: CleanupKind) -> bool {
    false
}

// Linux can return ENOENT or ESRCH for the same /proc read during exit.
fn proc_entry_disappeared(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn process_is_zombie(process: &Path) -> bool {
    fs::read_to_string(process.join("stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, fields)| fields.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
}

fn read_proc_file_limited(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_PROC_INSPECTION_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PROC_INSPECTION_BYTES {
        return Err(io::Error::other("process inspection output exceeded limit"));
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn mapped_path_hits(maps: &[u8], candidate: &Path) -> Result<bool> {
    for line in maps
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let mut index = 0;
        for _ in 0..5 {
            while index < line.len() && line[index].is_ascii_whitespace() {
                index += 1;
            }
            let start = index;
            while index < line.len() && !line[index].is_ascii_whitespace() {
                index += 1;
            }
            if start == index {
                bail!("invalid process maps entry")
            }
        }
        while index < line.len() && line[index].is_ascii_whitespace() {
            index += 1;
        }
        let path = &line[index..];
        if path.starts_with(b"/") {
            let path = path.strip_suffix(b" (deleted)").unwrap_or(path);
            if reference_hits(Path::new(std::ffi::OsStr::from_bytes(path)), candidate) {
                return Ok(true);
            }
            if path.windows(4).any(|window| window == b"\\012") {
                let decoded = path.split(|byte| *byte == b'\\').enumerate().fold(
                    Vec::new(),
                    |mut output, (index, part)| {
                        if index == 0 {
                            output.extend_from_slice(part);
                        } else if let Some(rest) = part.strip_prefix(b"012") {
                            output.push(b'\n');
                            output.extend_from_slice(rest);
                        } else {
                            output.push(b'\\');
                            output.extend_from_slice(part);
                        }
                        output
                    },
                );
                if reference_hits(Path::new(std::ffi::OsStr::from_bytes(&decoded)), candidate) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

#[cfg(not(target_os = "linux"))]
fn mapped_path_hits(_maps: &[u8], _candidate: &Path) -> Result<bool> {
    bail!("process map inspection is only available on Linux")
}

#[cfg(target_os = "linux")]
fn process_start_time(process: &Path, expected_uid: u32) -> Result<String> {
    if fs::metadata(process)?.uid() != expected_uid {
        bail!("process owner changed")
    }
    let stat = fs::read_to_string(process.join("stat"))?;
    let fields = stat.rsplit_once(')').context("invalid process stat")?.1;
    Ok(fields
        .split_ascii_whitespace()
        .nth(19)
        .context("missing process start time")?
        .to_owned())
}

#[cfg(target_os = "linux")]
fn privileged_process_references(
    process: &Path,
    uid: u32,
    candidate: &Path,
    repository: Option<&Path>,
    kind: CleanupKind,
    cache: &mut PrivilegedProcessCache,
) -> Result<bool> {
    let before = process_start_time(process, uid)?;
    cache.references_or_collect(process, &before, candidate, repository, kind, || {
        let links = run_readonly_proc_command(
            "/usr/bin/find",
            &[
                process.join("cwd").into_os_string(),
                process.join("root").into_os_string(),
                process.join("exe").into_os_string(),
                process.join("fd").into_os_string(),
                "-maxdepth".into(),
                "1".into(),
                "-type".into(),
                "l".into(),
                "-printf".into(),
                "%l\\0".into(),
            ],
        )?;
        let maps =
            run_readonly_proc_command("/usr/bin/cat", &[process.join("maps").into_os_string()])?;
        let command_line =
            run_readonly_proc_command("/usr/bin/cat", &[process.join("cmdline").into_os_string()])?;
        if before != process_start_time(process, uid)? {
            bail!("process identity changed during inspection")
        }
        Ok(PrivilegedProcessSnapshot {
            start_time: before.clone(),
            links,
            maps,
            command_line,
        })
    })
}

#[cfg(target_os = "linux")]
fn snapshot_references(
    snapshot: &PrivilegedProcessSnapshot,
    candidate: &Path,
    repository: Option<&Path>,
    kind: CleanupKind,
) -> Result<bool> {
    for link in snapshot
        .links
        .split(|byte| *byte == 0)
        .filter(|link| !link.is_empty())
    {
        let path = Path::new(std::ffi::OsStr::from_bytes(link));
        if reference_hits(path, candidate)
            || repository.is_some_and(|root| reference_hits(path, root))
        {
            return Ok(true);
        }
    }
    Ok(command_line_hits(&snapshot.command_line, candidate)
        || mapped_path_hits(&snapshot.maps, candidate)?
        || active_tool_may_use_cache(kind, &snapshot.command_line)
        || repository.is_some_and(|root| command_line_hits(&snapshot.command_line, root))
        || if let Some(root) = repository {
            mapped_path_hits(&snapshot.maps, root)?
        } else {
            false
        })
}

#[cfg(not(target_os = "linux"))]
fn privileged_process_references(
    _process: &Path,
    _uid: u32,
    _candidate: &Path,
    _repository: Option<&Path>,
    _kind: CleanupKind,
    _cache: &mut PrivilegedProcessCache,
) -> Result<bool> {
    bail!("privileged process inspection is only available on Linux")
}

#[cfg(target_os = "linux")]
fn run_readonly_proc_command(program: &str, arguments: &[std::ffi::OsString]) -> Result<Vec<u8>> {
    let mut args: Vec<std::ffi::OsString> = ["5s", "/usr/bin/sudo", "-n", program]
        .into_iter()
        .map(Into::into)
        .collect();
    args.extend_from_slice(arguments);
    let output = crate::host::run_bounded_cleanup_command(
        "/usr/bin/timeout",
        args,
        Duration::from_secs(6),
        MAX_PROC_INSPECTION_BYTES as usize,
    )
    .context("read-only process inspection unavailable")?;
    if output.stdout_truncated || output.stderr_truncated {
        bail!("process inspection output exceeded limit")
    }
    if !output.status.success() {
        bail!("read-only process inspection failed; non-interactive sudo may be unavailable")
    }
    Ok(output.stdout)
}

fn reference_hits(reference: &Path, candidate: &Path) -> bool {
    reference == candidate || reference.starts_with(candidate)
}

fn quarantine_and_remove(path: &Path, expected_device: u64, expected_inode: u64) -> Result<()> {
    let parent = path.parent().context("candidate has no parent directory")?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("invalid candidate name")?;
    for sequence in 0..100_u32 {
        let quarantine = parent.join(format!(
            ".guardwsl-trash-{}-{sequence}-{name}",
            std::process::id()
        ));
        revalidate_directory_identity(path, expected_device, expected_inode).with_context(
            || {
                format!(
                    "candidate changed immediately before quarantine: {}",
                    path.display()
                )
            },
        )?;
        match rename_noreplace(path, &quarantine) {
            Ok(()) => {
                purge_verified_quarantine(&quarantine, expected_device, expected_inode)?;
                return Ok(());
            }
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("could not reserve a quarantine name")
}

#[cfg(target_os = "linux")]
fn revalidate_directory_identity(
    path: &Path,
    expected_device: u64,
    expected_inode: u64,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not revalidate {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "identity mismatch at {}: not a real directory",
            path.display()
        )
    }
    if metadata.dev() != expected_device || metadata.ino() != expected_inode {
        bail!(
            "identity mismatch at {}: expected dev/inode {expected_device}/{expected_inode}, found {}/{}",
            path.display(),
            metadata.dev(),
            metadata.ino()
        )
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn purge_verified_quarantine(
    quarantine: &Path,
    expected_device: u64,
    expected_inode: u64,
) -> Result<()> {
    if let Err(error) = revalidate_directory_identity(quarantine, expected_device, expected_inode) {
        bail!(
            "purge refused; quarantine preserved at {}: {error:#}",
            quarantine.display()
        )
    }
    fs::remove_dir_all(quarantine).with_context(|| {
        format!(
            "quarantine preserved after purge failure: {}",
            quarantine.display()
        )
    })
}

#[cfg(target_os = "linux")]
fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in source path"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in destination path"))?;
    // SAFETY: both C strings are NUL-terminated, remain alive during the call,
    // and renameat2 does not retain either pointer after returning.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GIB;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::PermissionsExt as _;
    use tempfile::tempdir;

    fn fresh_cache_fixture() -> (tempfile::TempDir, GuardConfig, Repository) {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(
            root.join(".gitignore"),
            b"/.turbo\n/target\n/.next\n/node_modules\n",
        )
        .unwrap();
        fs::write(root.join("Cargo.toml"), b"[workspace]\n").unwrap();
        fs::write(root.join("package.json"), b"{}").unwrap();
        fs::write(root.join("yarn.lock"), b"# fixture\n").unwrap();
        for name in [".turbo", "target", ".next", "node_modules", "dist"] {
            fs::create_dir(root.join(name)).unwrap();
            fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(root.join(name).join("artifact"), b"regenerable").unwrap();
        }
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths.clear();
        config.cleanup.scan_roots = vec![root.clone()];
        (directory, config, Repository { root })
    }

    // Build only a temporary repository plan: never execute HOME discovery in a test.
    fn fresh_cache_plan(
        config: &GuardConfig,
        repository: &Repository,
        policy: CacheAgePolicy,
    ) -> CleanupPlan {
        let mut plan = CleanupPlan {
            created_at: Timestamp::now(),
            pressure: DiskPressure::Healthy,
            candidates: vec![],
            skips: vec![],
        };
        add_repository_candidates(
            config,
            false,
            policy,
            repository,
            &mut plan,
            &mut PlanningState::default(),
        );
        plan
    }

    #[test]
    fn manual_recent_cache_cleanup_is_scoped_audited_and_idempotent() {
        let (directory, config, repository) = fresh_cache_fixture();
        for _ in 0..2 {
            let retained = fresh_cache_plan(&config, &repository, CacheAgePolicy::Retention);
            assert!(retained.candidates.is_empty());
            assert!(repository.root.join(".turbo/artifact").exists());
        }
        let plan = fresh_cache_plan(&config, &repository, CacheAgePolicy::IncludeRecent);
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].kind, CleanupKind::ProjectCache);
        assert_eq!(plan.candidates[0].minimum_age_hours, 0);
        let audit = AuditLog::at(directory.path().join("audit.jsonl"));
        let dry_run = execute_cleanup(&config, &plan, CleanupMode::DryRun, &audit).unwrap();
        assert_eq!(dry_run.deleted_logical_bytes, 0);
        assert!(repository.root.join(".turbo/artifact").exists());
        let executed = execute_cleanup(&config, &plan, CleanupMode::Execute, &audit).unwrap();
        assert_eq!(executed.failures, 0);
        assert!(executed.deleted_logical_bytes > 0);
        assert!(!repository.root.join(".turbo").exists());
        for name in ["target", ".next", "node_modules", "dist"] {
            assert!(repository.root.join(name).join("artifact").exists());
        }
        let records = audit.tail(10).unwrap();
        assert!(
            records
                .iter()
                .any(|record| record.outcome == AuditOutcome::Planned
                    && record.detail.contains("minimum_age_hours=0"))
        );
        assert!(
            records
                .iter()
                .any(|record| record.outcome == AuditOutcome::Success)
        );
        let retried = fresh_cache_plan(&config, &repository, CacheAgePolicy::IncludeRecent);
        assert!(retried.candidates.is_empty());
        assert_eq!(
            execute_cleanup(&config, &retried, CleanupMode::Execute, &audit)
                .unwrap()
                .deleted_logical_bytes,
            0
        );
    }

    #[test]
    fn manual_recent_cache_cleanup_preserves_protected_and_open_files() {
        let (_directory, mut config, repository) = fresh_cache_fixture();
        let artifact = repository.root.join(".turbo/artifact");
        config.cleanup.protected_paths.push(artifact.clone());
        assert!(
            fresh_cache_plan(&config, &repository, CacheAgePolicy::IncludeRecent)
                .candidates
                .is_empty()
        );
        config.cleanup.protected_paths.clear();
        let held_open = fs::File::open(&artifact).unwrap();
        let blocked = fresh_cache_plan(&config, &repository, CacheAgePolicy::IncludeRecent);
        assert!(blocked.candidates.is_empty());
        assert!(
            blocked
                .skips
                .iter()
                .any(|skip| skip.reason.contains("active process"))
        );
        drop(held_open);
        assert_eq!(
            fresh_cache_plan(&config, &repository, CacheAgePolicy::IncludeRecent)
                .candidates
                .len(),
            1
        );
        assert_eq!(fs::read(&artifact).unwrap(), b"regenerable");
    }

    #[test]
    fn exited_proc_entries_and_inspection_errors_are_distinct() {
        for code in [libc::ENOENT, libc::ESRCH] {
            assert!(proc_entry_disappeared(&io::Error::from_raw_os_error(code)));
        }
        for code in [libc::EACCES, libc::EPERM, libc::EIO] {
            assert!(!proc_entry_disappeared(&io::Error::from_raw_os_error(code)));
        }
    }

    #[test]
    fn process_opened_after_planning_is_preserved_on_execution() {
        let (directory, config, repository) = fresh_cache_fixture();
        let plan = fresh_cache_plan(&config, &repository, CacheAgePolicy::IncludeRecent);
        assert_eq!(plan.candidates.len(), 1);
        let artifact = repository.root.join(".turbo/artifact");
        let held_open = fs::File::open(&artifact).unwrap();
        let audit = AuditLog::at(directory.path().join("audit.jsonl"));
        let report = execute_cleanup(&config, &plan, CleanupMode::Execute, &audit).unwrap();
        assert_eq!(report.deleted_logical_bytes, 0);
        assert!(artifact.exists());
        assert!(
            report
                .actions
                .iter()
                .any(|action| action.outcome == CleanupActionOutcome::Skipped)
        );
        drop(held_open);
    }

    #[test]
    fn process_opened_after_audit_is_preserved_before_quarantine() {
        let (directory, config, repository) = fresh_cache_fixture();
        let plan = fresh_cache_plan(&config, &repository, CacheAgePolicy::IncludeRecent);
        assert_eq!(plan.candidates.len(), 1, "{:?}", plan.skips);
        let audit = AuditLog::at(directory.path().join("audit.jsonl"));
        let mut held_open = None;
        let report =
            execute_cleanup_with_hook(&config, &plan, CleanupMode::Execute, &audit, |candidate| {
                held_open = Some(fs::File::open(candidate.path.join("artifact")).unwrap());
            })
            .unwrap();
        assert!(repository.root.join(".turbo/artifact").exists());
        assert_eq!(report.deleted_logical_bytes, 0);
        assert!(report.actions.iter().any(|action| {
            action.outcome == CleanupActionOutcome::Skipped
                && action.detail.contains("immediately before removal")
        }));
        assert!(held_open.is_some());
    }

    #[test]
    fn planning_deduplicates_identical_candidate_contexts() {
        let (_directory, config, repository) = fresh_cache_fixture();
        let mut plan = CleanupPlan {
            created_at: Timestamp::now(),
            pressure: DiskPressure::Healthy,
            candidates: Vec::new(),
            skips: Vec::new(),
        };
        let mut state = PlanningState::default();
        for _ in 0..2 {
            push_candidate(
                &config,
                repository.root.join(".turbo"),
                CleanupKind::ProjectCache,
                Some(&repository.root),
                0,
                &mut plan,
                &mut state,
            );
        }
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(state.seen.len(), 1);
    }

    #[test]
    fn planning_limit_preserves_remaining_candidates_and_reports_once() {
        let (_directory, config, repository) = fresh_cache_fixture();
        let mut plan = CleanupPlan {
            created_at: Timestamp::now(),
            pressure: DiskPressure::Healthy,
            candidates: Vec::new(),
            skips: Vec::new(),
        };
        let mut state = PlanningState::default();
        for number in 0..MAX_PLANNING_CANDIDATES {
            state.seen.insert((
                PathBuf::from(format!("/synthetic/{number}")),
                CleanupKind::ProjectCache,
                None,
                0,
            ));
        }
        for _ in 0..2 {
            push_candidate(
                &config,
                repository.root.join(".turbo"),
                CleanupKind::ProjectCache,
                Some(&repository.root),
                0,
                &mut plan,
                &mut state,
            );
        }
        assert!(plan.candidates.is_empty());
        assert_eq!(plan.skips.len(), 1);
        assert!(plan.skips[0].reason.contains("planning limit"));
        assert!(repository.root.join(".turbo/artifact").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn privileged_snapshot_is_reused_only_for_the_same_process_identity() {
        use std::cell::Cell;

        let mut cache = PrivilegedProcessCache::default();
        let calls = Cell::new(0);
        let process = Path::new("/proc/123");
        let collect = |start: &str| {
            calls.set(calls.get() + 1);
            Ok(PrivilegedProcessSnapshot {
                start_time: start.to_owned(),
                links: b"/tmp/first/cache/file\0".to_vec(),
                maps: Vec::new(),
                command_line: b"daemon\0".to_vec(),
            })
        };
        assert!(
            cache
                .references_or_collect(
                    process,
                    "11",
                    Path::new("/tmp/first/cache"),
                    None,
                    CleanupKind::ProjectCache,
                    || collect("11")
                )
                .unwrap()
        );
        assert!(
            !cache
                .references_or_collect(
                    process,
                    "11",
                    Path::new("/tmp/other/cache"),
                    None,
                    CleanupKind::ProjectCache,
                    || collect("11")
                )
                .unwrap()
        );
        assert_eq!(calls.get(), 1);
        assert!(
            cache
                .references_or_collect(
                    process,
                    "12",
                    Path::new("/tmp/first/cache"),
                    None,
                    CleanupKind::ProjectCache,
                    || collect("12")
                )
                .unwrap()
        );
        assert_eq!(calls.get(), 2);
        cache.max_bytes = 0;
        assert!(
            cache
                .references_or_collect(
                    process,
                    "13",
                    Path::new("/tmp/first/cache"),
                    None,
                    CleanupKind::ProjectCache,
                    || collect("13")
                )
                .unwrap()
        );
        assert!(cache.entries.is_empty());
        assert!(
            cache
                .references_or_collect(
                    process,
                    "13",
                    Path::new("/tmp/first/cache"),
                    None,
                    CleanupKind::ProjectCache,
                    || collect("13")
                )
                .unwrap()
        );
        assert_eq!(calls.get(), 4);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn privileged_snapshot_errors_are_not_cached() {
        use std::cell::Cell;

        let mut cache = PrivilegedProcessCache::default();
        let calls = Cell::new(0);
        for _ in 0..2 {
            let result = cache.references_or_collect(
                Path::new("/proc/123"),
                "11",
                Path::new("/tmp/cache"),
                None,
                CleanupKind::ProjectCache,
                || {
                    calls.set(calls.get() + 1);
                    bail!("inspection unavailable")
                },
            );
            assert!(result.is_err());
        }
        assert_eq!(calls.get(), 2);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn manual_recent_cache_cleanup_revalidates_changes_after_planning() {
        let (directory, config, repository) = fresh_cache_fixture();
        let plan = fresh_cache_plan(&config, &repository, CacheAgePolicy::IncludeRecent);
        assert_eq!(plan.candidates.len(), 1);
        fs::write(
            repository.root.join(".turbo/new-file"),
            b"preserve new work",
        )
        .unwrap();
        let audit = AuditLog::at(directory.path().join("audit.jsonl"));
        let result = execute_cleanup(&config, &plan, CleanupMode::Execute, &audit).unwrap();
        assert_eq!(result.deleted_logical_bytes, 0);
        assert_eq!(result.actions[0].outcome, CleanupActionOutcome::Skipped);
        assert!(repository.root.join(".turbo/new-file").exists());
    }

    #[test]
    fn manual_recent_cache_cleanup_preserves_tracked_unignored_and_symlinked_paths() {
        let (directory, config, repository) = fresh_cache_fixture();
        assert!(
            Command::new("git")
                .args(["add", "-f", ".turbo/artifact"])
                .current_dir(&repository.root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            fresh_cache_plan(&config, &repository, CacheAgePolicy::IncludeRecent)
                .candidates
                .is_empty()
        );
        assert!(
            Command::new("git")
                .args(["rm", "--cached", "-q", ".turbo/artifact"])
                .current_dir(&repository.root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(
            repository.root.join(".gitignore"),
            b"/target\n/.next\n/node_modules\n",
        )
        .unwrap();
        assert!(
            fresh_cache_plan(&config, &repository, CacheAgePolicy::IncludeRecent)
                .candidates
                .is_empty()
        );
        fs::write(
            repository.root.join(".gitignore"),
            b"/.turbo\n/target\n/.next\n/node_modules\n",
        )
        .unwrap();
        assert_eq!(
            fresh_cache_plan(&config, &repository, CacheAgePolicy::IncludeRecent)
                .candidates
                .len(),
            1
        );
        fs::rename(
            repository.root.join(".turbo"),
            directory.path().join("preserved-cache"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            repository.root.join("preserved-cache"),
            repository.root.join(".turbo"),
        )
        .unwrap();
        assert!(
            fresh_cache_plan(&config, &repository, CacheAgePolicy::IncludeRecent)
                .candidates
                .is_empty()
        );
        assert_eq!(
            fs::read(repository.root.join("preserved-cache/artifact")).unwrap(),
            b"regenerable"
        );
    }

    fn age(path: &Path) {
        let status = Command::new("touch")
            .args(["-d", "40 days ago"])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn generic_output_names_are_never_candidates() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        for name in ["dist", "build", "out"] {
            fs::create_dir(root.join(name)).unwrap();
            age(&root.join(name));
        }
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths.clear();
        config.cleanup.scan_roots = vec![root.clone()];
        let plan = plan_cleanup(&config, DiskPressure::Critical, true).unwrap();
        for name in ["dist", "build", "out"] {
            assert!(
                plan.candidates
                    .iter()
                    .all(|candidate| candidate.path != root.join(name))
            );
        }
    }

    #[test]
    fn preserved_quarantine_is_never_traversed() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let quarantine = root.join(".guardwsl-trash-42-0-target");
        fs::create_dir_all(quarantine.join("target")).unwrap();
        fs::write(quarantine.join("Cargo.toml"), b"[workspace]\n").unwrap();

        assert!(is_guard_quarantine_path(&quarantine));
        assert!(discover_project_roots(&root).is_empty());
    }

    #[test]
    fn nested_repositories_own_their_project_outputs() {
        let temp = tempfile::tempdir().unwrap();
        let outer = temp.path();
        let owned = outer.join("workspace");
        fs::create_dir(&owned).unwrap();
        fs::write(owned.join("Cargo.toml"), "[workspace]").unwrap();
        for (name, worktree) in [("checkout", false), ("linked", true)] {
            let nested = outer.join(name);
            fs::create_dir(&nested).unwrap();
            if worktree {
                fs::write(nested.join(".git"), "gitdir: /elsewhere").unwrap();
            } else {
                fs::create_dir(nested.join(".git")).unwrap();
            }
            fs::write(nested.join("package.json"), "{}").unwrap();
            let cache = nested.join(".next");
            fs::create_dir(&cache).unwrap();
            assert_eq!(
                discover_project_roots(&nested),
                std::slice::from_ref(&nested)
            );
            assert!(
                validate_git_state(outer, &cache)
                    .unwrap_err()
                    .to_string()
                    .contains("repository boundary")
            );
        }
        assert_eq!(discover_project_roots(outer), [owned]);
    }

    #[test]
    fn git_metadata_inside_an_output_preserves_the_whole_tree() {
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("vendor-copy");
        fs::create_dir(&checkout).unwrap();
        fs::write(checkout.join(".git"), "gitdir: ../nested").unwrap();
        assert!(profile_tree(temp.path()).is_err());
        assert!(checkout.join(".git").is_file());
    }

    #[test]
    fn healthy_maintenance_discovers_old_repository_artifacts() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let repo = fs::canonicalize(directory.path()).unwrap();
        Command::new("git")
            .args(["-c", "safe.directory=*", "init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        fs::write(repo.join(".gitignore"), b"/target\n").unwrap();
        fs::write(
            repo.join("Cargo.toml"),
            b"[package]\nname='x'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::create_dir(repo.join("target")).unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(repo.join("target"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(repo.join("target/artifact"), vec![0_u8; 16]).unwrap();
        age(&repo.join("target/artifact"));
        age(&repo.join("target"));
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths.clear();
        config.cleanup.scan_roots = vec![repo.clone()];
        let plan = plan_cleanup(&config, DiskPressure::Healthy, false).unwrap();
        let target = plan
            .candidates
            .iter()
            .find(|item| item.path == repo.join("target") && item.kind == CleanupKind::RustTarget)
            .unwrap_or_else(|| {
                panic!(
                    "healthy maintenance should include an old target. candidates: {:?}, skips: {:?}",
                    plan.candidates, plan.skips
                )
            });
        assert_eq!(target.minimum_age_hours, config.cleanup.build_min_age_hours);
    }

    #[test]
    fn pressure_and_need_space_compress_age_windows() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let repo = fs::canonicalize(directory.path()).unwrap();
        Command::new("git")
            .args(["-c", "safe.directory=*", "init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        fs::write(repo.join(".gitignore"), b"/target\n").unwrap();
        fs::write(
            repo.join("Cargo.toml"),
            b"[package]\nname='x'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::create_dir(repo.join("target")).unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(repo.join("target"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(repo.join("target/artifact"), vec![0_u8; 16]).unwrap();
        age(&repo.join("target/artifact"));
        age(&repo.join("target"));
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths.clear();
        config.cleanup.scan_roots = vec![repo.clone()];
        config.cleanup.critical_min_age_hours = 2;

        let healthy = plan_cleanup(&config, DiskPressure::Healthy, false).unwrap();
        let healthy_target = healthy
            .candidates
            .iter()
            .find(|item| item.path == repo.join("target"))
            .expect("healthy plan keeps the target");
        assert_eq!(
            healthy_target.minimum_age_hours,
            config.cleanup.build_min_age_hours
        );

        let pressured = plan_cleanup(&config, DiskPressure::Pressure, false).unwrap();
        let pressured_target = pressured
            .candidates
            .iter()
            .find(|item| item.path == repo.join("target"))
            .expect("pressure plan keeps the target");
        assert_eq!(
            pressured_target.minimum_age_hours,
            config.cleanup.critical_min_age_hours
        );

        let short_on_space = plan_cleanup(&config, DiskPressure::Healthy, true).unwrap();
        let short_target = short_on_space
            .candidates
            .iter()
            .find(|item| item.path == repo.join("target"))
            .expect("below-target plan keeps the target");
        assert_eq!(
            short_target.minimum_age_hours,
            config.cleanup.critical_min_age_hours
        );
    }

    #[test]
    fn early_reclaim_requires_critical_pressure_and_a_known_toolchain_cache() {
        let limit = 2_u64 << 30;
        let path = Path::new("/cache");
        for pressure in [DiskPressure::Healthy, DiskPressure::Pressure] {
            assert!(!qualifies_for_early_toolchain_reclaim(
                pressure,
                CleanupKind::GoCache,
                path,
                limit,
                limit,
            ));
        }
        for kind in [
            CleanupKind::GoCache,
            CleanupKind::RustCache,
            CleanupKind::RustTarget,
        ] {
            assert!(qualifies_for_early_toolchain_reclaim(
                DiskPressure::Critical,
                kind,
                path,
                limit,
                limit,
            ));
        }
        for kind in [
            CleanupKind::ProjectCache,
            CleanupKind::JavaScriptCache,
            CleanupKind::NextBuild,
            CleanupKind::NodeModules,
            CleanupKind::ToolCache,
        ] {
            assert!(!qualifies_for_early_toolchain_reclaim(
                DiskPressure::Emergency,
                kind,
                path,
                limit,
                limit,
            ));
        }
        assert!(qualifies_for_early_toolchain_reclaim(
            DiskPressure::Emergency,
            CleanupKind::ToolCache,
            Path::new("/user/.cache/sccache"),
            limit,
            limit,
        ));
        for (bytes, threshold) in [(limit - 1, limit), (limit, 0)] {
            assert!(!qualifies_for_early_toolchain_reclaim(
                DiskPressure::Critical,
                CleanupKind::GoCache,
                path,
                bytes,
                threshold,
            ));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn critical_recent_target_revalidates_size_and_process_before_removal() {
        let directory = tempdir().unwrap();
        let repo = fs::canonicalize(directory.path()).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        fs::write(repo.join(".gitignore"), b"/target\n").unwrap();
        fs::write(
            repo.join("Cargo.toml"),
            b"[package]\nname='sample'\nversion='0.1.0'\n",
        )
        .unwrap();
        let target = repo.join("target");
        fs::create_dir(&target).unwrap();
        let object = target.join("object");
        fs::write(&object, vec![0xA5; 256 * 1024]).unwrap();
        let mut config = GuardConfig::default();
        config.cleanup.critical_toolchain_min_bytes = 128 * 1024;
        let inspect = || {
            inspect_candidate_with_cache(
                &config,
                &target,
                CleanupKind::RustTarget,
                Some(&repo),
                1,
                DiskPressure::Critical,
                &mut PrivilegedProcessCache::default(),
            )
            .unwrap()
        };
        let candidate = inspect();
        assert!(candidate.early_toolchain_reclaim);
        let mut plan = CleanupPlan {
            created_at: Timestamp::now(),
            pressure: DiskPressure::Critical,
            candidates: vec![candidate],
            skips: Vec::new(),
        };
        let audit = AuditLog::at(repo.join("audit.jsonl"));

        fs::File::create(&object)
            .unwrap()
            .set_len(1024 * 1024)
            .unwrap();
        let sparse = profile_tree(&target).unwrap();
        assert!(sparse.bytes > config.cleanup.critical_toolchain_min_bytes);
        assert!(sparse.allocated_bytes < config.cleanup.critical_toolchain_min_bytes);
        let report = execute_cleanup(&config, &plan, CleanupMode::Execute, &audit).unwrap();
        assert_eq!(report.actions[0].outcome, CleanupActionOutcome::Skipped);
        assert!(report.actions[0].detail.contains("minimum age"));

        fs::write(&object, vec![0xA5; 256 * 1024]).unwrap();
        plan.candidates[0] = inspect();
        let mut process = None;
        let busy = execute_cleanup_with_hook(&config, &plan, CleanupMode::Execute, &audit, |_| {
            process = Some(
                Command::new("sleep")
                    .arg("15")
                    .current_dir(&target)
                    .spawn()
                    .unwrap(),
            );
        });
        let mut child = process.unwrap();
        let _ = child.kill();
        let _ = child.wait();
        let busy = busy.unwrap();
        assert_eq!(busy.actions[0].outcome, CleanupActionOutcome::Skipped);
        assert!(busy.actions[0].detail.contains("in use immediately"));
        assert!(object.exists());

        let idle = execute_cleanup(&config, &plan, CleanupMode::Execute, &audit).unwrap();
        assert_eq!(idle.actions[0].outcome, CleanupActionOutcome::Removed);
        assert!(!target.exists());
    }

    #[test]
    fn tool_cache_kind_is_an_exact_global_allowlist_entry() {
        assert_eq!(CleanupKind::ToolCache.as_str(), "tool_cache");
        assert_eq!(CleanupKind::ToolCache.risk_rank(), 0);
        assert!(validate_context(Path::new("/tmp/sccache"), CleanupKind::ToolCache, None).is_ok());
    }

    #[test]
    fn allowlist_names_cover_every_cleanup_kind() {
        let names = CleanupKind::allowlist_names();
        assert_eq!(names.len(), CleanupKind::ALL.len());
        for (kind, name) in CleanupKind::ALL.iter().zip(names.iter()) {
            assert_eq!(kind.as_str(), *name);
        }
        assert!(names.contains(&"tool_cache"));
        assert!(names.contains(&"javascript_cache"));
        assert!(names.contains(&"node_modules"));
    }

    #[test]
    fn dry_run_never_mutates_candidate() {
        let (directory, config, repository) = fresh_cache_fixture();
        let path = repository.root.join(".turbo");
        let plan = fresh_cache_plan(&config, &repository, CacheAgePolicy::IncludeRecent);
        assert_eq!(plan.candidates.len(), 1, "{:?}", plan.skips);
        let audit = AuditLog::at(directory.path().join("audit.jsonl"));
        #[cfg(target_os = "linux")]
        let identity_before = {
            let metadata = fs::symlink_metadata(&path).unwrap();
            (metadata.dev(), metadata.ino())
        };
        let report = execute_cleanup(&config, &plan, CleanupMode::DryRun, &audit).unwrap();
        assert!(path.exists());
        assert_eq!(fs::read(path.join("artifact")).unwrap(), b"regenerable");
        #[cfg(target_os = "linux")]
        {
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert_eq!((metadata.dev(), metadata.ino()), identity_before);
        }
        assert_eq!(report.deleted_logical_bytes, 0);
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].outcome, CleanupActionOutcome::WouldRemove);
        let records = audit.tail(10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, AuditOutcome::Planned);
        assert_eq!(records[0].path.as_deref(), Some(path.as_path()));
        assert_eq!(
            records[0].estimated_bytes,
            Some(plan.candidates[0].estimated_bytes)
        );
    }

    #[test]
    fn protected_symlink_is_compared_by_its_canonical_target() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let real = directory.path().join("real");
        let candidate = real.join("cache");
        fs::create_dir_all(&candidate).unwrap();
        #[cfg(target_os = "linux")]
        {
            fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700)).unwrap();
            std::os::unix::fs::symlink(&real, directory.path().join("alias")).unwrap();
        }
        let candidate = fs::canonicalize(candidate).unwrap();
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths = vec![directory.path().join("alias/cache")];
        assert!(
            inspect_candidate(&config, &candidate, CleanupKind::JavaScriptCache, None, 1).is_err()
        );
    }

    #[test]
    fn inaccessible_protected_root_keeps_its_lexical_protection() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let protected = directory.path().join("protected");
        fs::create_dir(&protected).unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o000)).unwrap();
        let nested = protected.join("nested");
        let normalized = normalize_protected_path(&nested).unwrap();
        assert_eq!(normalized, nested);
        #[cfg(target_os = "linux")]
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nondumpable_process_fixture() {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;

        if std::env::var_os("GUARD_TEST_NONDUMPABLE").is_none() {
            return;
        }
        let _held_file = std::env::var_os("GUARD_TEST_HELD_FILE")
            .map(|path| fs::File::open(path).expect("open synthetic fixture"));
        let mapping = std::env::var_os("GUARD_TEST_MAPPED_FILE").map(|path| {
            let file = fs::File::open(path).expect("open synthetic mapping fixture");
            // SAFETY: the fixture is nonempty, read-only, and unmapped before exit.
            let mapping = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    9,
                    libc::PROT_READ,
                    libc::MAP_PRIVATE,
                    file.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(mapping, libc::MAP_FAILED);
            mapping
        });
        // SAFETY: this changes only the synthetic child process.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) }, 0);
        println!("GUARD_TEST_PROCESS_READY");
        std::io::stdout().flush().unwrap();
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input).unwrap();
        if let Some(mapping) = mapping {
            // SAFETY: this address and length are the successful mmap above.
            assert_eq!(unsafe { libc::munmap(mapping, 9) }, 0);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a working non-interactive sudo for read-only proc inspection"]
    fn nondumpable_process_only_blocks_the_cache_it_references() {
        use std::io::{BufRead, BufReader};

        struct Fixture(std::process::Child);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        assert!(
            Command::new("/usr/bin/sudo")
                .args(["-n", "true"])
                .status()
                .unwrap()
                .success()
        );
        let temporary = tempdir().unwrap();
        let cache = temporary.path().join("cache");
        fs::create_dir(&cache).unwrap();
        let file = cache.join("fixture");
        fs::write(&file, b"synthetic").unwrap();
        let cache = fs::canonicalize(cache).unwrap();
        for mode in ["unrelated", "open-fd", "mapped-only"] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "cleanup::tests::nondumpable_process_fixture",
                    "--nocapture",
                ])
                .env("GUARD_TEST_NONDUMPABLE", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            if mode == "open-fd" {
                command.env("GUARD_TEST_HELD_FILE", &file);
            } else if mode == "mapped-only" {
                command.env("GUARD_TEST_MAPPED_FILE", &file);
            }
            let mut child = Fixture(command.spawn().unwrap());
            let mut output = BufReader::new(child.0.stdout.take().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                assert_ne!(
                    output.read_line(&mut line).unwrap(),
                    0,
                    "fixture terminated"
                );
                if line.contains("GUARD_TEST_PROCESS_READY") {
                    break;
                }
            }
            assert_eq!(
                path_is_in_use(&cache, None, CleanupKind::ProjectCache).unwrap(),
                mode != "unrelated",
                "mode={mode}"
            );
            drop(child.0.stdin.take());
            assert!(child.0.wait().unwrap().success());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a working non-interactive sudo for read-only proc inspection"]
    fn active_repository_working_directory_protects_only_its_artifacts() {
        struct Fixture(std::process::Child);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let temp = tempdir().unwrap();
        let active = temp.path().join("active");
        let inactive = temp.path().join("inactive");
        for root in [&active, &inactive] {
            fs::create_dir_all(root.join(".next")).unwrap();
        }
        let _child = Fixture(
            Command::new("/bin/sleep")
                .arg("30")
                .current_dir(&active)
                .spawn()
                .unwrap(),
        );
        assert!(
            path_is_in_use(&active.join(".next"), Some(&active), CleanupKind::NextBuild).unwrap()
        );
        assert!(
            !path_is_in_use(
                &inactive.join(".next"),
                Some(&inactive),
                CleanupKind::NextBuild
            )
            .unwrap()
        );
    }

    #[test]
    fn active_package_tools_protect_only_their_cache_family() {
        assert!(active_tool_may_use_cache(
            CleanupKind::GoCache,
            b"/usr/local/go/bin/go\0test\0"
        ));
        assert!(!active_tool_may_use_cache(
            CleanupKind::RustCache,
            b"/usr/local/go/bin/go\0test\0"
        ));
        assert!(active_tool_may_use_cache(
            CleanupKind::JavaScriptCache,
            b"node\0/usr/lib/npm/bin/npm-cli.js\0install\0"
        ));
        assert!(active_tool_may_use_cache(
            CleanupKind::RustCache,
            b"/home/user/.cargo/bin/cargo\0check\0"
        ));
        assert!(!active_tool_may_use_cache(
            CleanupKind::GoCache,
            b"/usr/bin/gopher\0"
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_go_cache_does_not_claim_the_idle_default() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let proc = temp.path().join("process");
        let candidate = temp.path().join(".cache/go-build");
        let alternate = temp.path().join("isolated");
        fs::create_dir_all(proc.join("ns")).unwrap();
        fs::create_dir_all(candidate.join("nested")).unwrap();
        fs::create_dir(&alternate).unwrap();
        symlink("/usr/bin/go", proc.join("exe")).unwrap();
        symlink("/proc/self/ns/mnt", proc.join("ns/mnt")).unwrap();
        symlink("/proc/self/root", proc.join("root")).unwrap();
        let alias = temp.path().join("alias");
        symlink(&candidate, &alias).unwrap();

        let separate = || go_uses_separate_build_cache(&proc, &candidate, CleanupKind::GoCache);
        assert!(!separate());
        for (setting, expected) in [
            (format!("GOCACHE={}", alternate.display()), true),
            (
                format!("GOCACHE={}\0GOCACHEPROG=custom", alternate.display()),
                false,
            ),
            (format!("GOCACHE={}", candidate.display()), false),
            (format!("GOCACHE={}/nested", alias.display()), false),
            (format!("GOCACHE={}", temp.path().display()), false),
            ("GOCACHE=relative".into(), false),
            ("UNRELATED=1".into(), false),
        ] {
            fs::write(proc.join("environ"), setting).unwrap();
            assert_eq!(separate(), expected);
        }
        fs::write(
            proc.join("environ"),
            format!("GOCACHE={}", alternate.display()),
        )
        .unwrap();
        assert!(!go_uses_separate_build_cache(
            &proc,
            &temp.path().join("go/pkg/mod/cache"),
            CleanupKind::GoCache
        ));
        fs::remove_file(proc.join("ns/mnt")).unwrap();
        assert!(!separate());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mapped_paths_preserve_spaces_and_path_boundaries() {
        let maps = b"1000-2000 r--p 0 00:00 0 /tmp/cache other/file\n2000-3000 rw-p 0 00:00 0 [heap]\n3000-4000 r--p 0 00:00 0 /tmp/cache/file with spaces (deleted)\n";
        assert!(mapped_path_hits(maps, Path::new("/tmp/cache")).unwrap());
        assert!(!mapped_path_hits(maps, Path::new("/tmp/cache-other")).unwrap());
        assert!(mapped_path_hits(b"invalid\n", Path::new("/tmp/cache")).is_err());
        let escaped = b"1000-2000 r--p 0 00:00 0 /tmp/cache\\012name/file\n";
        assert!(mapped_path_hits(escaped, Path::new("/tmp/cache\nname")).unwrap());
        assert!(mapped_path_hits(escaped, Path::new("/tmp/cache\\012name")).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn command_line_detects_exact_cache_arguments_without_prefix_confusion() {
        let candidate = Path::new("/tmp/cache");
        assert!(command_line_hits(
            b"tool\0--target=/tmp/cache/debug\0",
            candidate
        ));
        assert!(!command_line_hits(
            b"tool\0--target=/tmp/cache-other\0",
            candidate
        ));
        let non_utf8 = std::ffi::OsStr::from_bytes(b"/tmp/\xff-cache");
        assert!(command_line_hits(
            b"tool\0/tmp/\xff-cache/file\0",
            Path::new(non_utf8)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn readonly_process_helper_fails_closed_when_unavailable() {
        assert!(run_readonly_proc_command("/nonexistent-guard-test-command", &[]).is_err());
        assert!(process_start_time(Path::new("/proc/0"), effective_uid()).is_err());
    }

    #[test]
    fn report_marks_uninspectable_process_as_cleanup_blocked() {
        let now = Timestamp::now();
        let mut report = CleanupReport {
            started_at: now,
            finished_at: now,
            mode: CleanupMode::Execute,
            pressure: DiskPressure::Emergency,
            planned_logical_bytes: 0,
            deleted_logical_bytes: 0,
            actions: Vec::new(),
            planning_skips: Vec::new(),
            failures: 0,
        };
        assert!(!report.process_inspection_blocked());
        report.planning_skips.push(CleanupSkip {
            path: PathBuf::from("/cache"),
            reason: format!("{PROCESS_INSPECTION_ERROR_PREFIX}42: unavailable"),
        });
        assert!(report.process_inspection_blocked());
        report.planning_skips.clear();
        report.actions.push(CleanupAction {
            path: PathBuf::from("/cache"),
            kind: CleanupKind::GoCache,
            outcome: CleanupActionOutcome::Skipped,
            logical_bytes: 0,
            detail: format!("{PROCESS_INSPECTION_ERROR_PREFIX}42: unavailable"),
        });
        assert!(report.process_inspection_blocked());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn writable_candidate_root_fails_closed() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("cache");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        let config = GuardConfig::default();
        let error =
            inspect_candidate(&config, &path, CleanupKind::JavaScriptCache, None, 1).unwrap_err();
        assert!(error.to_string().contains("private root"));
        assert!(path.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quarantine_rejects_identity_change_before_rename() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("cache");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("artifact"), b"preserve").unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();

        let error = quarantine_and_remove(&path, metadata.dev(), metadata.ino() ^ 1).unwrap_err();

        assert!(error.to_string().contains("immediately before"));
        assert_eq!(fs::read(path.join("artifact")).unwrap(), b"preserve");
        assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".guardwsl-trash-")
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn purge_preserves_quarantine_when_identity_diverges() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let quarantine = directory.path().join(".guardwsl-trash-test");
        fs::create_dir(&quarantine).unwrap();
        fs::write(quarantine.join("artifact"), b"preserve").unwrap();
        let metadata = fs::symlink_metadata(&quarantine).unwrap();

        let error =
            purge_verified_quarantine(&quarantine, metadata.dev(), metadata.ino() ^ 1).unwrap_err();

        assert!(error.to_string().contains("quarantine preserved"));
        assert!(
            error
                .to_string()
                .contains(&quarantine.display().to_string())
        );
        assert_eq!(fs::read(quarantine.join("artifact")).unwrap(), b"preserve");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quarantine_removes_only_the_expected_identity() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("cache");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("artifact"), b"regenerable").unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();

        quarantine_and_remove(&path, metadata.dev(), metadata.ino()).unwrap();

        assert!(!path.exists());
        assert!(fs::read_dir(directory.path()).unwrap().next().is_none());
    }

    #[test]
    fn build_report_uses_logical_not_physical_reclaimed_bytes() {
        let report = CleanupReport {
            started_at: Timestamp::now(),
            finished_at: Timestamp::now(),
            mode: CleanupMode::Execute,
            pressure: DiskPressure::Pressure,
            planned_logical_bytes: GIB,
            deleted_logical_bytes: GIB,
            actions: Vec::new(),
            planning_skips: Vec::new(),
            failures: 0,
        };
        assert!(report.succeeded());
        assert_eq!(report.deleted_logical_bytes, GIB);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn internal_hard_links_inside_tree_are_deletable() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("target");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join("artifact"), b"content").unwrap();
        fs::hard_link(path.join("artifact"), path.join("artifact-link")).unwrap();
        age(&path.join("artifact"));
        age(&path);

        let profile = profile_tree(&path).unwrap();
        assert!(profile.bytes > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn external_hard_link_refuses_tree() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("target");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join("artifact"), b"content").unwrap();
        let outside = directory.path().join("outside-link");
        fs::hard_link(path.join("artifact"), &outside).unwrap();
        age(&path.join("artifact"));
        age(&path);

        let error = profile_tree(&path).unwrap_err();
        assert!(error.to_string().contains("hard link"));
        assert!(error.to_string().contains("outside"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dual_lockfiles_still_prove_reproducibility() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let repo = fs::canonicalize(directory.path()).unwrap();
        Command::new("git")
            .args(["-c", "safe.directory=*", "init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        fs::write(repo.join(".gitignore"), b"/node_modules\n").unwrap();
        fs::write(repo.join("package.json"), b"{}").unwrap();
        fs::write(repo.join("package-lock.json"), b"{}").unwrap();
        fs::write(repo.join("yarn.lock"), b"").unwrap();
        fs::create_dir(repo.join("node_modules")).unwrap();
        fs::set_permissions(repo.join("node_modules"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(repo.join("node_modules/x"), b"x").unwrap();
        age(&repo.join("node_modules/x"));
        age(&repo.join("node_modules"));
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths.clear();
        config.cleanup.scan_roots = vec![repo.clone()];
        let plan = plan_cleanup(&config, DiskPressure::Healthy, false).unwrap();
        assert!(
            plan.candidates
                .iter()
                .any(|item| item.path == repo.join("node_modules")),
            "dual lockfiles should still allow node_modules. skips: {:?}",
            plan.skips
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_lockfile_still_refuses_node_modules() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let repo = fs::canonicalize(directory.path()).unwrap();
        Command::new("git")
            .args(["-c", "safe.directory=*", "init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        fs::write(repo.join(".gitignore"), b"/node_modules\n").unwrap();
        fs::write(repo.join("package.json"), b"{}").unwrap();
        fs::create_dir(repo.join("node_modules")).unwrap();
        fs::set_permissions(repo.join("node_modules"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(repo.join("node_modules/x"), b"x").unwrap();
        age(&repo.join("node_modules/x"));
        age(&repo.join("node_modules"));
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths.clear();
        config.cleanup.scan_roots = vec![repo.clone()];
        let plan = plan_cleanup(&config, DiskPressure::Healthy, false).unwrap();
        assert!(
            !plan
                .candidates
                .iter()
                .any(|item| item.path == repo.join("node_modules")),
            "node_modules without any lockfile must be refused"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn monorepo_root_lockfile_unlocks_nested_node_modules() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let repo = fs::canonicalize(directory.path()).unwrap();
        Command::new("git")
            .args(["-c", "safe.directory=*", "init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        fs::write(
            repo.join(".gitignore"),
            b"/apps/web/node_modules\n/apps/web/.next\n",
        )
        .unwrap();
        fs::write(repo.join("package.json"), b"{}").unwrap();
        fs::write(repo.join("yarn.lock"), b"").unwrap();
        let app = repo.join("apps/web");
        fs::create_dir_all(&app).unwrap();
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(app.join("package.json"), b"{}").unwrap();
        fs::create_dir(app.join("node_modules")).unwrap();
        fs::set_permissions(app.join("node_modules"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(app.join("node_modules/x"), b"x").unwrap();
        age(&app.join("node_modules/x"));
        age(&app.join("node_modules"));
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths.clear();
        config.cleanup.scan_roots = vec![repo.clone()];
        let plan = plan_cleanup(&config, DiskPressure::Healthy, false).unwrap();
        assert!(
            plan.candidates
                .iter()
                .any(|item| item.path == app.join("node_modules")),
            "monorepo root lockfile should unlock nested node_modules. skips: {:?}",
            plan.skips
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ancestor_walk_stops_at_repository_root() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let outer = directory.path().join("outer");
        fs::create_dir(&outer).unwrap();
        fs::set_permissions(&outer, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(outer.join("yarn.lock"), b"").unwrap();
        let repo = outer.join("repo");
        fs::create_dir(&repo).unwrap();
        fs::set_permissions(&repo, fs::Permissions::from_mode(0o700)).unwrap();
        let project = repo.join("app");
        fs::create_dir(&project).unwrap();
        assert!(!find_lockfile(&project, Some(&repo)));
    }

    #[test]
    fn missing_lockfile_without_git_only_checks_parent() {
        let directory = tempdir().unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let parent = directory.path().join("app");
        fs::create_dir(&parent).unwrap();
        #[cfg(target_os = "linux")]
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(directory.path().join("yarn.lock"), b"").unwrap();
        assert!(!find_lockfile(&parent, None));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn skip_reason_distinguishes_manifest_and_lockfile() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let repo = fs::canonicalize(directory.path()).unwrap();
        Command::new("git")
            .args(["-c", "safe.directory=*", "init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        fs::write(repo.join(".gitignore"), b"/node_modules\n").unwrap();
        fs::create_dir(repo.join("node_modules")).unwrap();
        fs::set_permissions(repo.join("node_modules"), fs::Permissions::from_mode(0o700)).unwrap();
        let config = GuardConfig::default();
        let error = validate_context(
            &repo.join("node_modules"),
            CleanupKind::NodeModules,
            Some(&repo),
        )
        .unwrap_err();
        assert!(error.to_string().contains("package.json"));

        fs::write(repo.join("package.json"), b"{}").unwrap();
        let error = validate_context(
            &repo.join("node_modules"),
            CleanupKind::NodeModules,
            Some(&repo),
        )
        .unwrap_err();
        assert!(error.to_string().contains("lockfile"));
        let _ = config;
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn group_writable_parent_is_rejected_even_with_primary_gid() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let parent = directory.path().join("project");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o775)).unwrap();
        let candidate = parent.join("cache");
        fs::create_dir(&candidate).unwrap();
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(candidate.join("x"), b"x").unwrap();
        age(&candidate.join("x"));
        age(&candidate);
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths.clear();
        let error = inspect_candidate(&config, &candidate, CleanupKind::JavaScriptCache, None, 1)
            .unwrap_err();
        assert!(error.to_string().contains("unsafe owner or mode"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn other_writable_parent_still_rejected() {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let parent = directory.path().join("project");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        let candidate = parent.join("cache");
        fs::create_dir(&candidate).unwrap();
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(candidate.join("x"), b"x").unwrap();
        age(&candidate.join("x"));
        age(&candidate);
        let mut config = GuardConfig::default();
        config.cleanup.protected_paths.clear();
        let error = inspect_candidate(&config, &candidate, CleanupKind::JavaScriptCache, None, 1)
            .unwrap_err();
        assert!(error.to_string().contains("unsafe owner or mode"));
    }
}
