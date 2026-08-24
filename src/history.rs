//! Append-only audit trail for the v1 core.

use crate::fsutil::{append_private, default_state_dir, read_private};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_HISTORY_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Planned,
    Success,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub at: DateTime<Utc>,
    pub event: String,
    pub outcome: AuditOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_bytes: Option<u64>,
}

impl AuditRecord {
    #[must_use]
    pub fn new(event: impl Into<String>, outcome: AuditOutcome, detail: impl Into<String>) -> Self {
        Self {
            at: Utc::now(),
            event: event.into(),
            outcome,
            path: None,
            detail: detail.into(),
            estimated_bytes: None,
        }
    }

    #[must_use]
    pub fn for_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_estimated_bytes(mut self, bytes: u64) -> Self {
        self.estimated_bytes = Some(bytes);
        self
    }
}

#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub fn discover() -> Result<Self> {
        Ok(Self::at(default_state_dir()?.join("audit.jsonl")))
    }

    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, record: &AuditRecord) -> Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        append_private(&self.path, &line)
            .with_context(|| format!("could not audit {}", record.event))
    }

    pub fn tail(&self, limit: usize) -> Result<Vec<AuditRecord>> {
        if limit == 0 || !self.path.exists() {
            return Ok(Vec::new());
        }
        let bytes = read_private(&self.path, MAX_HISTORY_BYTES)?;
        let mut records = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .rev()
            .take(limit)
            .map(serde_json::from_slice)
            .collect::<std::result::Result<Vec<AuditRecord>, _>>()?;
        records.reverse();
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn records_round_trip_in_order() {
        let directory = tempdir().unwrap();
        let log = AuditLog::at(directory.path().join("audit.jsonl"));
        log.append(&AuditRecord::new(
            "cleanup",
            AuditOutcome::Planned,
            "before",
        ))
        .unwrap();
        log.append(&AuditRecord::new("cleanup", AuditOutcome::Success, "after"))
            .unwrap();
        let records = log.tail(10).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].outcome, AuditOutcome::Planned);
        assert_eq!(records[1].outcome, AuditOutcome::Success);
    }
}
