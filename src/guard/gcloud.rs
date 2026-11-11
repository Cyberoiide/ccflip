//! gcloud CLI accounts (`gcloud auth list`) and application-default
//! credentials (ADC).
//!
//! gcloud mints its own 1 h access tokens on demand from a long-lived
//! refresh token, so there is nothing to renew ahead of time and no expiry to
//! schedule on (the hourly backstop is the poll). The job is noticing a
//! refresh token that died — revoked, password change, Workspace session
//! control — and asking a human. Never opens a browser unasked: a gcloud
//! login always needs a click.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Deserialize;

use super::{Account, Provider, Run, State, ask_login, clear_needs_you, needs_you, on_path};
use crate::cswap;

pub struct Gcloud;

/// The ADC "account": `gcloud auth application-default …`.
const ADC: &str = "application-default";

#[derive(Deserialize)]
struct Listed {
    account: String,
    #[serde(default)]
    status: String,
}

/// (account, worth asking a human about): the active account and ADC are
/// what tools use implicitly; other logged-in accounts are only reported.
fn accounts() -> Vec<(String, bool)> {
    if !on_path("gcloud") {
        return Vec::new();
    }
    let listed: Vec<Listed> = Command::new("gcloud")
        .args(["auth", "list", "--format=json"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice(&o.stdout).ok())
        .unwrap_or_default();
    let mut out: Vec<(String, bool)> = listed
        .into_iter()
        .map(|l| (l.account, l.status == "ACTIVE"))
        .collect();
    let config = env::var_os("CLOUDSDK_CONFIG")
        .filter(|v| !v.is_empty())
        .map_or_else(|| cswap::home().join(".config/gcloud"), PathBuf::from);
    // Existence only: the file holds a refresh token, never read it here.
    if config
        .join("application_default_credentials.json")
        .is_file()
    {
        out.push((ADC.to_string(), true));
    }
    out
}

/// Ask gcloud for a token (it refreshes if needed) and classify the answer.
/// The token itself goes to /dev/null.
fn probe(account: &str) -> State {
    let mut cmd = Command::new("timeout");
    cmd.args(["30", "gcloud", "auth"]);
    if account == ADC {
        cmd.args(["application-default", "print-access-token"]);
    } else {
        // One argv element: an account starting with `-` stays a value.
        cmd.args(["print-access-token", &format!("--account={account}")]);
    }
    let Ok(out) = cmd.stdout(Stdio::null()).output() else {
        return State::Error;
    };
    if out.status.success() {
        return State::Ok;
    }
    dead_credentials(&String::from_utf8_lossy(&out.stderr))
}

/// gcloud ends every "these credentials are gone" error (invalid_grant,
/// reauth required, no credentials) with the login command to run; network
/// and tool failures never mention it.
fn dead_credentials(stderr: &str) -> State {
    if ["gcloud auth login", "gcloud auth application-default login"]
        .iter()
        .any(|fix| stderr.contains(fix))
    {
        State::Expired
    } else {
        State::Error
    }
}

impl Provider for Gcloud {
    fn id(&self) -> &'static str {
        "gcloud"
    }

    fn label(&self) -> &'static str {
        "gcloud"
    }

    fn check(&self, _run: &mut Run) {
        for (account, ask) in accounts() {
            match probe(&account) {
                State::Ok => clear_needs_you(self, &account),
                State::Error => {
                    println!("sso-guard: gcloud {account}: could not check, retrying next run");
                }
                State::Expired | State::Expiring if ask => {
                    println!(
                        "sso-guard: gcloud {account} credentials are dead, asking for a login"
                    );
                    let _ = fs::write(needs_you(self, &account), "");
                    ask_login(self, &account);
                }
                State::Expired | State::Expiring => println!(
                    "sso-guard: gcloud {account} credentials are dead (not the active account; gcloud auth login {account})"
                ),
            }
        }
    }

    fn status(&self, _run: &Run) -> Vec<Account> {
        accounts()
            .into_iter()
            .map(|(account, _)| Account {
                provider: self.id(),
                state: probe(&account),
                expires_at: None,
                needs_you: needs_you(self, &account).exists(),
                account,
            })
            .collect()
    }

    fn login(&self, account: &str) -> bool {
        let mut cmd = Command::new("gcloud");
        if account == ADC {
            cmd.args(["auth", "application-default", "login"]);
        } else {
            cmd.args(["auth", "login", account]);
        }
        cmd.status().is_ok_and(|s| s.success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim gcloud 593 stderr.
    #[test]
    fn classifies_real_gcloud_errors() {
        let dead = "ERROR: (gcloud.auth.print-access-token) There was a problem refreshing your current auth tokens: ('invalid_grant: Bad Request', {'error': 'invalid_grant', 'error_description': 'Bad Request'})\nPlease run:\n\n  $ gcloud auth login\n\nto obtain new credentials.";
        let none = "ERROR: (gcloud.auth.print-access-token) Your current active account [ghost@example.com] does not have any valid credentials\nPlease run:\n\n  $ gcloud auth login\n";
        let adc = "ERROR: (gcloud.auth.application-default.print-access-token) There was a problem refreshing your current auth tokens: ('invalid_grant: Bad Request', ...)\nPlease run:\n\n  $ gcloud auth application-default login\n";
        let offline = "ERROR: (gcloud.auth.print-access-token) HTTPSConnectionPool(host='oauth2.googleapis.com', port=443): Max retries exceeded with url: /token";
        assert_eq!(dead_credentials(dead), State::Expired);
        assert_eq!(dead_credentials(none), State::Expired);
        assert_eq!(dead_credentials(adc), State::Expired);
        assert_eq!(dead_credentials(offline), State::Error);
        assert_eq!(dead_credentials(""), State::Error);
    }
}
