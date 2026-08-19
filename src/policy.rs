//! `~/.config/ccflip/policy.env` — the host's standing declaration:
//! containment (which accounts are pinned-only) and the numeric knobs.
//!
//! Read fresh at every use, by BOTH the launcher and the watcher — the README
//! promises "set either in the environment or in policy.env", and a knob that
//! only the daemon honours is a knob that silently does nothing. Not sourced:
//! parsed, so a name in the file can never clobber our own state and a
//! deleted line takes effect immediately.

use std::env;
use std::fs;
use std::path::PathBuf;

use crate::{cswap, launch};

pub struct Policy {
    vars: Vec<(String, String)>,
}

pub fn read() -> Policy {
    let path = env::var_os("CCFLIP_POLICY")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| cswap::home().join(".config/ccflip/policy.env"));
    Policy {
        vars: fs::read_to_string(path)
            .map(|text| launch::parse_env_file(&text))
            .unwrap_or_default(), // no file = no policy
    }
}

impl Policy {
    fn get(&self, key: &str) -> Option<String> {
        self.vars
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    /// Numeric knob: the environment wins (an ad-hoc override for one run),
    /// then policy.env, then the built-in default.
    pub fn knob<T: std::str::FromStr + std::fmt::Display>(&self, key: &str, default: T) -> T {
        match self.get(key) {
            Some(raw) if env::var_os(key).is_none() => raw.parse().unwrap_or_else(|_| {
                eprintln!("ccflip: {key}={raw} in policy.env is not a number, using {default}");
                default
            }),
            _ => crate::env_parsed(key, default),
        }
    }

    /// (pinned-only accounts, the account the default login is forced back to).
    pub fn containment(&self) -> Option<(Vec<String>, Option<String>)> {
        let raw = self.get("CCFLIP_PINNED_ONLY")?;
        if raw.contains(',') {
            eprintln!(
                "ccflip: CCFLIP_PINNED_ONLY is space-separated; commas will match nothing: {raw}"
            );
        }
        let pinned: Vec<String> = raw.split_whitespace().map(str::to_string).collect();
        (!pinned.is_empty()).then(|| (pinned, self.get("CCFLIP_DEFAULT_ACCOUNT")))
    }
}

#[cfg(test)]
mod tests {
    use super::Policy;

    fn policy(pairs: &[(&str, &str)]) -> Policy {
        Policy {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn knob_comes_from_policy_file_then_default() {
        let p = policy(&[("CCFLIP_SPEND_THRESHOLD", "80")]);
        assert_eq!(p.knob("CCFLIP_SPEND_THRESHOLD", 97.0), 80.0);
        assert_eq!(p.knob("CCFLIP_SPEND_ALERT_PCT", 90.0), 90.0);
        // Unparseable values never silently change routing.
        assert_eq!(policy(&[("X", "high")]).knob("X", 12.0), 12.0);
    }

    #[test]
    fn containment_needs_a_pinned_list() {
        assert_eq!(policy(&[]).containment(), None);
        assert_eq!(policy(&[("CCFLIP_PINNED_ONLY", "  ")]).containment(), None);
        assert_eq!(
            policy(&[
                ("CCFLIP_PINNED_ONLY", "a@x b@x"),
                ("CCFLIP_DEFAULT_ACCOUNT", "c@x"),
            ])
            .containment(),
            Some((vec!["a@x".into(), "b@x".into()], Some("c@x".into())))
        );
    }
}
