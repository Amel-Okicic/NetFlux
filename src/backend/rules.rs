//! Rule persistence: load/save `HashMap<String, Rule>` as JSON at
//! `%APPDATA%\NetFlux\rules.json`.
//!
//! Rules are keyed by lowercase executable path (the same key the flow table
//! and GUI use). A missing file is treated as an empty rule set; parse errors
//! are logged and also fall back to empty so a corrupt file never crashes the
//! backend.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::types::{GlobalLimits, Rule};

/// Directory that holds our persisted state: `%APPDATA%\NetFlux`.
pub fn config_dir() -> PathBuf {
    // APPDATA is always set for interactive users on Windows. If it is somehow
    // absent (unusual service contexts), fall back to the current directory so
    // we still have somewhere to write.
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("NetFlux")
}

/// Full path to the per-process rules file.
pub fn rules_path() -> PathBuf {
    config_dir().join("rules.json")
}

/// Full path to the machine-wide bandwidth settings file.
pub fn global_limits_path() -> PathBuf {
    config_dir().join("global_limits.json")
}

/// Load machine-wide bandwidth ceilings. A missing or invalid file means unlimited.
pub fn load_global_limits() -> GlobalLimits {
    let path = global_limits_path();
    match std::fs::read_to_string(&path) {
        Ok(data) => match serde_json::from_str::<GlobalLimits>(&data) {
            Ok(limits) => limits,
            Err(e) => {
                tracing::warn!("failed to parse {}: {e}; using unlimited global rates", path.display());
                GlobalLimits::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => GlobalLimits::default(),
        Err(e) => {
            tracing::warn!("failed to read {}: {e}; using unlimited global rates", path.display());
            GlobalLimits::default()
        }
    }
}

/// Persist machine-wide bandwidth ceilings.
pub fn save_global_limits(limits: &GlobalLimits) -> anyhow::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = global_limits_path();
    std::fs::write(&path, serde_json::to_string_pretty(limits)?)?;
    tracing::debug!("saved global bandwidth limits to {}", path.display());
    Ok(())
}

/// Load the persisted rules. Missing file or any error => empty map.
pub fn load() -> HashMap<String, Rule> {
    let path = rules_path();
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("no rules file at {}, starting empty", path.display());
            return HashMap::new();
        }
        Err(e) => {
            tracing::warn!("failed to read {}: {e}", path.display());
            return HashMap::new();
        }
    };

    match serde_json::from_str::<HashMap<String, Rule>>(&data) {
        Ok(mut map) => {
            // Defensively normalise keys to lowercase so lookups from the engine
            // (which always lowercases) match regardless of how the file was written.
            map = map
                .into_iter()
                .map(|(k, mut v)| {
                    let key = k.to_lowercase();
                    v.exe_path = v.exe_path.to_lowercase();
                    (key, v)
                })
                .collect();

            let loaded_count = map.len();
            map.retain(|_, rule| rule.is_configured());
            let removed_count = loaded_count - map.len();
            if removed_count > 0 {
                tracing::info!(
                    "removed {} empty rule(s) from {}",
                    removed_count,
                    path.display()
                );
                if let Err(e) = save(&map) {
                    tracing::warn!("failed to persist cleaned rules file: {e}");
                }
            }

            tracing::info!("loaded {} configured rule(s) from {}", map.len(), path.display());
            map
        }
        Err(e) => {
            tracing::warn!("failed to parse {}: {e}; starting empty", path.display());
            HashMap::new()
        }
    }
}

/// Persist the given rules as pretty JSON, creating the directory if needed.
pub fn save(rules: &HashMap<String, Rule>) -> anyhow::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = rules_path();
    let json = serde_json::to_string_pretty(rules)?;
    std::fs::write(&path, json)?;
    tracing::debug!("saved {} rule(s) to {}", rules.len(), path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_round_trip_through_json() {
        let mut rules: HashMap<String, Rule> = HashMap::new();
        rules.insert(
            "c:\\program files\\steam\\steam.exe".to_string(),
            Rule {
                exe_path: "c:\\program files\\steam\\steam.exe".to_string(),
                down_limit: Some(2 * 1024 * 1024),
                up_limit: None,
                blocked: false,
                ..Rule::default()
            },
        );
        rules.insert(
            "c:\\windows\\system32\\curl.exe".to_string(),
            Rule {
                exe_path: "c:\\windows\\system32\\curl.exe".to_string(),
                down_limit: None,
                up_limit: Some(512 * 1024),
                blocked: true,
                ..Rule::default()
            },
        );

        let json = serde_json::to_string_pretty(&rules).expect("serialize");
        let back: HashMap<String, Rule> = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(rules, back);
    }

    #[test]
    fn empty_map_round_trips() {
        let rules: HashMap<String, Rule> = HashMap::new();
        let json = serde_json::to_string(&rules).unwrap();
        let back: HashMap<String, Rule> = serde_json::from_str(&json).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn global_limits_round_trip_through_json() {
        let limits = GlobalLimits {
            down_limit: Some(1024 * 1024),
            up_limit: Some(314_572),
        };
        let json = serde_json::to_string(&limits).unwrap();
        let back: GlobalLimits = serde_json::from_str(&json).unwrap();
        assert_eq!(limits, back);
    }

    #[test]
    fn legacy_empty_global_limits_default_to_unlimited() {
        let limits: GlobalLimits = serde_json::from_str("{}").unwrap();
        assert_eq!(limits, GlobalLimits::default());
    }

    #[test]
    fn rule_defaults_are_unlimited_and_unblocked() {
        let r = Rule::default();
        assert!(r.down_limit.is_none());
        assert!(r.up_limit.is_none());
        assert!(!r.blocked);
        assert!(r.expires_at.is_none());
        assert!(r.restore.is_none());
    }

    #[test]
    fn legacy_rule_json_defaults_to_permanent() {
        let json = r#"{
            "exe_path": "c:\\apps\\player.exe",
            "down_limit": 1024,
            "up_limit": null,
            "blocked": false
        }"#;
        let rule: Rule = serde_json::from_str(json).expect("legacy rule JSON");
        assert_eq!(rule.down_limit, Some(1024));
        assert!(rule.expires_at.is_none());
        assert!(rule.restore.is_none());
    }
}
