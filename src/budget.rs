//! `ccflip budget` — the default account's REAL dollar meters, plus this
//! host's month-to-date Bedrock consumption.
//!
//! cswap only parses extra_usage (the overflow budget); the seat's included
//! monthly pool arrives under an internal codename with `limit_dollars` —
//! any such bucket is printed generically. The watcher reuses the same fetch
//! for its spend alerts.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::cswap;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Credentials {
    claude_ai_oauth: Option<OauthCreds>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OauthCreds {
    access_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct OauthUsage {
    #[serde(default)]
    extra_usage: Option<ExtraUsage>,
    /// Every other top-level key; only `Bucket`-shaped ones are meters.
    #[serde(flatten)]
    rest: HashMap<String, Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Entry {
    Bucket(Bucket),
    Other(serde::de::IgnoredAny),
}

/// `limit_dollars` is the discriminator — a bucket with one IS a meter. The
/// rest is optional: a missing field must cost one column, never the whole
/// report (and never the alert that rides on it).
#[derive(Debug, Deserialize)]
struct Bucket {
    limit_dollars: f64,
    #[serde(default)]
    used_dollars: f64,
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExtraUsage {
    #[serde(default)]
    used_credits: f64,
    #[serde(default)]
    monthly_limit: f64,
    utilization: Option<f64>,
}

fn pct(utilization: Option<f64>, used: f64, limit: f64) -> f64 {
    utilization.unwrap_or(if limit > 0.0 {
        used / limit * 100.0
    } else {
        0.0
    })
}

impl Bucket {
    fn pct(&self) -> f64 {
        pct(self.utilization, self.used_dollars, self.limit_dollars)
    }
}

impl ExtraUsage {
    fn pct(&self) -> f64 {
        pct(self.utilization, self.used_credits, self.monthly_limit)
    }
}

impl OauthUsage {
    /// (meter name, utilization pct) for every dollar meter — what the
    /// watcher alerts on.
    pub fn meters(&self) -> Vec<(String, f64)> {
        let mut out: Vec<(String, f64)> = self
            .rest
            .iter()
            .filter_map(|(k, e)| match e {
                Entry::Bucket(b) => Some((k.clone(), b.pct())),
                Entry::Other(_) => None,
            })
            .collect();
        if let Some(x) = &self.extra_usage {
            out.push(("extra_usage".to_string(), x.pct()));
        }
        out
    }
}

/// GET the oauth usage endpoint with the default profile's token. Every
/// failure carries its reason: a silently-empty meter list would mean the
/// watcher stops guarding the budget with nothing in the journal.
pub fn fetch_usage() -> Result<OauthUsage> {
    let creds = fs::read_to_string(cswap::home().join(".claude/.credentials.json"))
        .context("reading ~/.claude/.credentials.json")?;
    let token = serde_json::from_str::<Credentials>(&creds)
        .context("parsing ~/.claude/.credentials.json")?
        .claude_ai_oauth
        .and_then(|o| o.access_token)
        .context("no oauth token in the default profile")?;
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build();
    let agent: ureq::Agent = config.into();
    let mut response = agent
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .call()
        .context("oauth usage request failed")?;
    let body = response
        .body_mut()
        .read_to_string()
        .context("reading oauth usage response")?;
    serde_json::from_str(&body).context("parsing oauth usage response")
}

pub fn run() {
    let mut rows: Vec<[String; 4]> = Vec::new();
    match fetch_usage() {
        Ok(usage) => {
            for (name, entry) in &usage.rest {
                if let Entry::Bucket(b) = entry {
                    rows.push([
                        name.clone(),
                        format!("${} / ${}", b.used_dollars, b.limit_dollars),
                        format!("{}%", b.pct().round()),
                        format!("resets {}", b.resets_at.as_deref().unwrap_or("?")),
                    ]);
                }
            }
            if let Some(x) = &usage.extra_usage {
                rows.push([
                    "extra_usage (overflow)".to_string(),
                    format!("${} / ${}", x.used_credits / 100.0, x.monthly_limit / 100.0),
                    format!("{}%", x.pct()),
                    "monthly".to_string(),
                ]);
            }
        }
        Err(e) => println!("oauth meters unavailable: {e:#}"),
    }

    // Bedrock tier: YOUR consumption only — the AWS account is shared and
    // Cost Explorer cannot split by IAM principal, so sum this machine's
    // transcripts instead. Bedrock-served messages carry msg_bdrk_ ids.
    // Tokens, not dollars — no invented pricing table.
    let (out_tok, in_tok, msgs) = bedrock_month_to_date();
    rows.push([
        "bedrock (you, this host)".to_string(),
        format!(
            "{:.2}M out / {:.0}M in+cache",
            out_tok as f64 / 1e6,
            in_tok as f64 / 1e6
        ),
        format!("{msgs} msgs"),
        "month-to-date".to_string(),
    ]);

    // column -t
    let mut widths = [0usize; 4];
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.len());
        }
    }
    for row in &rows {
        let mut line = String::new();
        for (w, cell) in widths.iter().zip(row) {
            line.push_str(&format!("{cell:<w$}  "));
        }
        println!("{}", line.trim_end());
    }
}

#[derive(Deserialize)]
struct TranscriptLine {
    message: Option<TranscriptMessage>,
}

#[derive(Deserialize)]
struct TranscriptMessage {
    id: Option<String>,
    usage: Option<TokenUsage>,
}

#[derive(Deserialize, Default)]
struct TokenUsage {
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

/// (output tokens, input+cache tokens, message count) from every Bedrock
/// message in this month's transcripts, across the default profile and all
/// pinned-session profiles.
fn bedrock_month_to_date() -> (u64, u64, u64) {
    let cutoff = month_start();
    let mut totals = (0u64, 0u64, 0u64);
    for root in [
        cswap::home().join(".claude/projects"),
        cswap::data_home().join("claude-swap/sessions"),
    ] {
        sum_dir(&root, cutoff, &mut totals);
    }
    totals
}

fn sum_dir(dir: &Path, cutoff: Option<SystemTime>, totals: &mut (u64, u64, u64)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            sum_dir(&path, cutoff, totals);
            continue;
        }
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        if let Some(cut) = cutoff
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|m| m < cut)
        {
            continue;
        }
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        // Transcripts run to gigabytes: stream, and only JSON-parse lines
        // that can possibly match — the substring scan is orders faster.
        for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
            let Ok(line) = line else { break };
            if !line.contains("msg_bdrk_") {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<TranscriptLine>(&line) else {
                continue;
            };
            let Some(msg) = parsed.message else { continue };
            if !msg.id.as_deref().unwrap_or("").starts_with("msg_bdrk_") {
                continue;
            }
            let u = msg.usage.unwrap_or_default();
            totals.0 += u.output_tokens;
            totals.1 += u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens;
            totals.2 += 1;
        }
    }
}

/// First of the current month as a SystemTime. std has no calendar, so ask
/// date(1) — this tool is Linux-only anyway (`who` reads /proc). `None`
/// (no date binary) counts everything rather than nothing.
fn month_start() -> Option<SystemTime> {
    let out = Command::new("sh")
        .args(["-c", r#"date -d "$(date +%Y-%m-01)" +%s"#])
        .output()
        .ok()?;
    let secs: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usage_buckets_generically() {
        let json = r#"{
            "some_internal_codename": {"used_dollars": 12.5, "limit_dollars": 250.0,
                                        "utilization": 5.0, "resets_at": "2026-09-01"},
            "unrelated_string": "ignore me",
            "unrelated_object": {"foo": 1},
            "extra_usage": {"used_credits": 150.0, "monthly_limit": 10000.0, "utilization": 1.5}
        }"#;
        let usage: OauthUsage = serde_json::from_str(json).unwrap();
        let mut meters = usage.meters();
        meters.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            meters,
            vec![
                ("extra_usage".to_string(), 1.5),
                ("some_internal_codename".to_string(), 5.0),
            ]
        );
    }

    #[test]
    fn counts_only_bedrock_messages() {
        let lines = r#"{"message":{"id":"msg_bdrk_1","usage":{"output_tokens":10,"input_tokens":5,"cache_read_input_tokens":100}}}
{"message":{"id":"msg_direct","usage":{"output_tokens":99,"input_tokens":99}}}
not json at all
{"noMessage":true}"#;
        let mut totals = (0u64, 0u64, 0u64);
        for line in lines.lines() {
            let Ok(parsed) = serde_json::from_str::<TranscriptLine>(line) else {
                continue;
            };
            let Some(msg) = parsed.message else { continue };
            if !msg.id.as_deref().unwrap_or("").starts_with("msg_bdrk_") {
                continue;
            }
            let u = msg.usage.unwrap_or_default();
            totals.0 += u.output_tokens;
            totals.1 += u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens;
            totals.2 += 1;
        }
        assert_eq!(totals, (10, 105, 1));
    }
}
