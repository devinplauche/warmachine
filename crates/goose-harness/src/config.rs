use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How a tool call is treated when it runs unattended.
///
/// Enforcement lands in Phase 4 (AC-4.1); this type is the schema the
/// enforcement, the approval flow, and the config file all share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionClass {
    AutoApproved,
    ApprovalRequired,
    Forbidden,
}

/// The `harness:` section of config.yaml.
///
/// Unknown keys are rejected so a misspelled policy key cannot silently
/// fall back to defaults.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessConfig {
    pub action_classes: ActionClasses,
    pub delivery: DeliveryConfig,
    pub memory: MemoryConfig,
    pub daemon: DaemonConfig,
}

/// Tool-name pattern lists per action class, plus the class applied when
/// no pattern matches.
///
/// Defaults are conservative: anything that sends, spends, deletes, or
/// runs shell commands requires approval, and unlisted tools require
/// approval too. Auto-approval only ever comes from patterns the user
/// lists explicitly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActionClasses {
    pub auto_approved: Vec<String>,
    pub approval_required: Vec<String>,
    pub forbidden: Vec<String>,
    pub default: ActionClass,
}

impl Default for ActionClasses {
    fn default() -> Self {
        Self {
            auto_approved: Vec::new(),
            approval_required: [
                "*send*",
                "*spend*",
                "*purchase*",
                "*delete*",
                "*remove*",
                "shell",
                "*exec*",
            ]
            .iter()
            .map(|pattern| pattern.to_string())
            .collect(),
            forbidden: Vec::new(),
            default: ActionClass::ApprovalRequired,
        }
    }
}

impl ActionClasses {
    /// First matching class wins, in the order forbidden,
    /// approval-required, auto-approved; otherwise the configured
    /// default applies.
    pub fn classify(&self, tool_name: &str) -> ActionClass {
        if matches_any(&self.forbidden, tool_name) {
            ActionClass::Forbidden
        } else if matches_any(&self.approval_required, tool_name) {
            ActionClass::ApprovalRequired
        } else if matches_any(&self.auto_approved, tool_name) {
            ActionClass::AutoApproved
        } else {
            self.default
        }
    }
}

fn matches_any(patterns: &[String], tool_name: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| glob_match(pattern, tool_name))
}

/// Minimal glob: `*` matches any run of characters, all other characters
/// match literally. Sufficient for tool-name patterns and avoids a new
/// dependency for the schema layer.
fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    glob_match_at(&pattern, &value)
}

fn glob_match_at(pattern: &[char], value: &[char]) -> bool {
    if pattern.is_empty() {
        return value.is_empty();
    }
    if pattern[0] == '*' {
        for skip in 0..=value.len() {
            if glob_match_at(&pattern[1..], &value[skip..]) {
                return true;
            }
        }
        return false;
    }
    if value.is_empty() || pattern[0] != value[0] {
        return false;
    }
    glob_match_at(&pattern[1..], &value[1..])
}

/// Proactive-delivery policy (AC-3.3). Enforcement lands in Phase 3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeliveryConfig {
    /// Master switch for proactive outbound messages.
    pub enabled: bool,
    /// Local quiet hours as "HH:MM"; deliveries are held while the local
    /// time is inside the window. Both unset disables quiet hours.
    pub quiet_hours_start: Option<String>,
    pub quiet_hours_end: Option<String>,
    /// Topics the user has opted out of entirely.
    pub topic_opt_outs: Vec<String>,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            quiet_hours_start: Some("22:00".to_string()),
            quiet_hours_end: Some("07:00".to_string()),
            topic_opt_outs: Vec::new(),
        }
    }
}

/// Memory service settings (AC-1). Consumer lands in Phase 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    pub enabled: bool,
    /// Upper bound on the session-start memory brief, in tokens.
    pub brief_max_tokens: u32,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            brief_max_tokens: 2000,
        }
    }
}

/// Daemon settings (AC-3). Consumer lands in Phase 3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Where the event trigger listens. Loopback only; see the design
    /// note (decision D6).
    pub webhook_bind: String,
    pub webhook_port: u16,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            webhook_bind: "127.0.0.1".to_string(),
            webhook_port: 8471,
        }
    }
}

/// Parse the `harness:` section out of a full config.yaml document.
/// A document without the section yields the default (conservative)
/// configuration.
pub fn parse_harness_config(config_yaml: &str) -> anyhow::Result<HarnessConfig> {
    let document: serde_yaml::Value = serde_yaml::from_str(config_yaml)?;
    let Some(section) = document.get("harness") else {
        return Ok(HarnessConfig::default());
    };
    let config: HarnessConfig = serde_yaml::from_value(section.clone())?;
    Ok(config)
}

/// Convenience for tests and tooling that keep the section in a map.
pub fn config_from_map(map: &BTreeMap<String, serde_yaml::Value>) -> anyhow::Result<HarnessConfig> {
    match map.get("harness") {
        Some(section) => Ok(serde_yaml::from_value(section.clone())?),
        None => Ok(HarnessConfig::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative() {
        let config = HarnessConfig::default();
        assert_eq!(config.action_classes.default, ActionClass::ApprovalRequired);
        assert!(config.action_classes.auto_approved.is_empty());
        assert_eq!(
            config.action_classes.classify("send_email"),
            ActionClass::ApprovalRequired
        );
        assert_eq!(
            config.action_classes.classify("delete_file"),
            ActionClass::ApprovalRequired
        );
        assert_eq!(
            config.action_classes.classify("shell"),
            ActionClass::ApprovalRequired
        );
        assert_eq!(
            config.action_classes.classify("read_file"),
            ActionClass::ApprovalRequired
        );
    }

    #[test]
    fn explicit_auto_approve_and_forbidden_win() {
        let classes = ActionClasses {
            auto_approved: vec!["read_*".to_string()],
            approval_required: vec![],
            forbidden: vec!["read_secrets".to_string()],
            default: ActionClass::ApprovalRequired,
        };
        assert_eq!(classes.classify("read_file"), ActionClass::AutoApproved);
        assert_eq!(classes.classify("read_secrets"), ActionClass::Forbidden);
    }

    #[test]
    fn missing_section_yields_defaults() {
        let config = parse_harness_config("providers: {}\n").unwrap();
        assert_eq!(config, HarnessConfig::default());
    }

    #[test]
    fn section_parses_and_unknown_keys_are_rejected() {
        let yaml = r#"
harness:
  memory:
    enabled: false
    brief_max_tokens: 500
"#;
        let config = parse_harness_config(yaml).unwrap();
        assert!(!config.memory.enabled);
        assert_eq!(config.memory.brief_max_tokens, 500);
        assert_eq!(config.delivery, DeliveryConfig::default());

        let bad = "harness:\n  memory:\n    enabeld: true\n";
        assert!(parse_harness_config(bad).is_err());
    }

    #[test]
    fn round_trips_through_yaml() {
        let config = HarnessConfig::default();
        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: HarnessConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(config, parsed);
    }
}
