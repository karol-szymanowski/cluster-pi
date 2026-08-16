use crate::error::AuditError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum AuditAction {
    Format,
    Mount,
    Unmount,
    EepromWrite,
    RoleChange,
    Evacuate,
    Demote,
    Promote,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum AuditPhase {
    /// Write-ahead record logged BEFORE the mutating syscall/action executes.
    PreMutation,
    /// Completion record logged AFTER the mutating syscall/action successfully completes.
    PostMutation,
    /// Failure record logged if the action failed.
    Failed,
}

/// A structured immutable audit record for destructive or physical system actions.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuditEntry {
    /// Timestamp of the audit event (UTC).
    pub timestamp: DateTime<Utc>,

    /// Hardware or block device serial.
    pub device_serial: String,

    /// Stable device path or partition ID if applicable.
    pub device_path: Option<String>,

    /// Category of action performed.
    pub action: AuditAction,

    /// Phase of the action (PreMutation, PostMutation, Failed).
    pub phase: AuditPhase,

    /// Identity/component that initiated the action.
    pub requested_by: String,

    /// Snapshot of the state before the mutation.
    pub before_state: serde_json::Value,

    /// Snapshot of the target or resulting state.
    pub after_state: serde_json::Value,

    /// Additional contextual attributes.
    pub details: Option<serde_json::Value>,
}

impl AuditEntry {
    pub fn builder(
        device_serial: impl Into<String>,
        action: AuditAction,
        requested_by: impl Into<String>,
    ) -> AuditEntryBuilder {
        AuditEntryBuilder {
            device_serial: device_serial.into(),
            device_path: None,
            action,
            phase: AuditPhase::PreMutation,
            requested_by: requested_by.into(),
            before_state: serde_json::Value::Null,
            after_state: serde_json::Value::Null,
            details: None,
        }
    }
}

pub struct AuditEntryBuilder {
    device_serial: String,
    device_path: Option<String>,
    action: AuditAction,
    phase: AuditPhase,
    requested_by: String,
    before_state: serde_json::Value,
    after_state: serde_json::Value,
    details: Option<serde_json::Value>,
}

impl AuditEntryBuilder {
    pub fn device_path(mut self, path: impl Into<String>) -> Self {
        self.device_path = Some(path.into());
        self
    }

    pub fn phase(mut self, phase: AuditPhase) -> Self {
        self.phase = phase;
        self
    }

    pub fn before_state(mut self, state: impl Serialize) -> Self {
        self.before_state = serde_json::to_value(state).unwrap_or(serde_json::Value::Null);
        self
    }

    pub fn after_state(mut self, state: impl Serialize) -> Self {
        self.after_state = serde_json::to_value(state).unwrap_or(serde_json::Value::Null);
        self
    }

    pub fn details(mut self, details: impl Serialize) -> Self {
        self.details = serde_json::to_value(details).ok();
        self
    }

    pub fn build(self) -> AuditEntry {
        AuditEntry {
            timestamp: Utc::now(),
            device_serial: self.device_serial,
            device_path: self.device_path,
            action: self.action,
            phase: self.phase,
            requested_by: self.requested_by,
            before_state: self.before_state,
            after_state: self.after_state,
            details: self.details,
        }
    }
}

/// Append-only JSON-lines audit log writer with mandatory write-ahead `fsync` semantics.
pub struct AuditLog {
    path: PathBuf,
    file_lock: Mutex<()>,
}

impl AuditLog {
    /// Opens or creates the append-only audit log at the given path.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, AuditError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Open/create file to ensure write permissions upfront
        let _ = OpenOptions::new().create(true).append(true).open(&path)?;

        Ok(Self {
            path,
            file_lock: Mutex::new(()),
        })
    }

    /// Appends an audit entry to the log and immediately executes `fsync` before returning.
    pub fn record(&self, entry: &AuditEntry) -> Result<(), AuditError> {
        let _guard = self.file_lock.lock().unwrap();

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;

        let line = serde_json::to_string(entry)?;
        writeln!(file, "{}", line)?;

        // Mandatory fsync to survive crashes/sudden power loss
        file.sync_all().map_err(|e| {
            AuditError::FsyncFailed(format!(
                "Failed to fsync audit record to {:?}: {}",
                self.path, e
            ))
        })?;

        tracing::info!(
            target: "cluster_common::audit",
            action = ?entry.action,
            phase = ?entry.phase,
            device = %entry.device_serial,
            requested_by = %entry.requested_by,
            "Audit record committed to disk"
        );

        Ok(())
    }

    /// Reads all recorded audit entries from the log.
    pub fn read_entries(&self) -> Result<Vec<AuditEntry>, AuditError> {
        let _guard = self.file_lock.lock().unwrap();

        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: AuditEntry = serde_json::from_str(&line)?;
            entries.push(entry);
        }

        Ok(entries)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_audit_write_and_read() {
        let temp_file = NamedTempFile::new().unwrap();
        let log_path = temp_file.path().to_path_buf();
        let audit = AuditLog::new(&log_path).unwrap();

        let pre_entry = AuditEntry::builder("SERIAL123", AuditAction::Format, "cluster-operator")
            .device_path("/dev/disk/by-id/nvme-test")
            .phase(AuditPhase::PreMutation)
            .before_state(serde_json::json!({"state": "Unformatted"}))
            .after_state(serde_json::json!({"state": "MountedEtcd"}))
            .build();

        audit.record(&pre_entry).unwrap();

        let post_entry = AuditEntry::builder("SERIAL123", AuditAction::Format, "cluster-operator")
            .device_path("/dev/disk/by-id/nvme-test")
            .phase(AuditPhase::PostMutation)
            .before_state(serde_json::json!({"state": "Unformatted"}))
            .after_state(serde_json::json!({"state": "MountedEtcd"}))
            .build();

        audit.record(&post_entry).unwrap();

        let entries = audit.read_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].phase, AuditPhase::PreMutation);
        assert_eq!(entries[1].phase, AuditPhase::PostMutation);
        assert_eq!(entries[0].device_serial, "SERIAL123");
    }
}
