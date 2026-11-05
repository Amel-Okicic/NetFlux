//! Shared types between the capture/limiter backend and the GUI.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Seconds of rate history kept per process for sparklines.
pub const HISTORY_LEN: usize = 60;

/// Previous permanent state to restore when a temporary rule expires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuleBackup {
    pub down_limit: Option<u64>,
    pub up_limit: Option<u64>,
    pub blocked: bool,
}

/// Independent computer-wide bandwidth ceilings. None means unlimited.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GlobalLimits {
    /// Maximum total download rate in bytes/sec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub down_limit: Option<u64>,
    /// Maximum total upload rate in bytes/sec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up_limit: Option<u64>,
}

/// A per-process traffic rule, matched by lowercase executable path.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    /// Full lowercase path of the executable this rule applies to.
    pub exe_path: String,
    /// Download limit in bytes/sec. `None` = unlimited.
    pub down_limit: Option<u64>,
    /// Upload limit in bytes/sec. `None` = unlimited.
    pub up_limit: Option<u64>,
    /// If true, all traffic for this process is dropped.
    pub blocked: bool,
    /// Unix timestamp (seconds) at which this temporary rule expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Permanent state to restore at expiry; `None` means remove the rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<RuleBackup>,
}

impl RuleBackup {
    /// Whether the saved state enforces a traffic limit or blocks the process.
    pub fn is_configured(&self) -> bool {
        self.blocked || self.down_limit.is_some() || self.up_limit.is_some()
    }
}

impl Rule {
    /// Whether this rule has an effect now or temporarily suspends a configured rule.
    pub fn is_configured(&self) -> bool {
        self.blocked
            || self.down_limit.is_some()
            || self.up_limit.is_some()
            || (self.expires_at.is_some()
                && self.restore.as_ref().is_some_and(RuleBackup::is_configured))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_permanent_rule_is_not_configured() {
        assert!(!Rule::default().is_configured());
    }

    #[test]
    fn empty_temporary_rule_without_a_restorable_rule_is_not_configured() {
        let rule = Rule {
            expires_at: Some(100),
            ..Rule::default()
        };
        assert!(!rule.is_configured());
    }

    #[test]
    fn temporary_suspension_of_a_configured_rule_is_configured() {
        let rule = Rule {
            expires_at: Some(100),
            restore: Some(RuleBackup {
                down_limit: Some(1_000),
                up_limit: None,
                blocked: false,
            }),
            ..Rule::default()
        };
        assert!(rule.is_configured());
    }
}

/// Live stats for one process, sent to the GUI once per second.
#[derive(Debug, Clone, Default)]
pub struct ProcessStats {
    pub pid: u32,
    /// Full executable path (may be empty if resolution failed).
    pub exe_path: String,
    /// File name portion, for display.
    pub name: String,
    /// Current download rate, bytes/sec.
    pub down_rate: u64,
    /// Current upload rate, bytes/sec.
    pub up_rate: u64,
    /// Total bytes since start.
    pub down_total: u64,
    pub up_total: u64,
    /// Rolling per-second download rates, newest last, up to HISTORY_LEN.
    pub down_history: Vec<u64>,
    /// Rolling per-second upload rates, newest last, up to HISTORY_LEN.
    pub up_history: Vec<u64>,
    /// Number of active flows attributed to this process.
    pub flow_count: usize,
}

/// Snapshot of everything the GUI needs, produced ~1x/sec by the backend.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Keyed by lowercase exe_path (processes aggregated across PIDs share a key).
    pub processes: HashMap<String, ProcessStats>,
    /// Total current rates across all processes, bytes/sec.
    pub total_down_rate: u64,
    pub total_up_rate: u64,
    /// Currently active rules, keyed by lowercase exe_path.
    pub rules: HashMap<String, Rule>,
    /// Active computer-wide download and upload ceilings.
    pub global_limits: GlobalLimits,
}

/// Commands sent from the GUI to the backend.
#[derive(Debug, Clone)]
pub enum Command {
    /// Insert or replace the rule for `rule.exe_path`.
    SetRule(Rule),
    /// Remove any rule for this lowercase exe path.
    RemoveRule(String),
    /// Replace the independent machine-wide download/upload ceilings.
    SetGlobalLimits(GlobalLimits),
    /// Stop the engine threads and flush state (app exit).
    Shutdown,
}
