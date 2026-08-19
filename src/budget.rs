//! `ccflip budget` — the default account's REAL dollar meters, plus this
//! host's month-to-date Bedrock consumption.
//!
//! cswap only parses extra_usage (the overflow budget); the seat's included
//! monthly pool arrives under an internal codename with `limit_dollars` —
//! any such bucket is printed generically. The watcher reuses the same fetch
//! for its spend alerts.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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
    // Cost Explorer cannot split by IAM principal, so price this machine's
    // transcripts (msg_bdrk_ ids) at API rates. Costs are cached per file by
    // mtime+size, so only changed files are re-parsed.
    rows.push([
        "bedrock (you, this host)".to_string(),
        format!("≈ ${:.2}", bedrock_month_cost()),
        "API-rate est.".to_string(),
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
    model: Option<String>,
    usage: Option<TokenUsage>,
}

#[derive(Deserialize, Default, Clone, Copy)]
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

/// Dollars per million tokens, (input, output). Verified against real
/// receipts; cache writes bill at 1.25x input and cache reads at 0.1x.
/// An unknown model is priced as Opus rather than free — a silent 0 would
/// under-report the one number this command exists to give.
const RATES: &[(&str, f64, f64)] = &[
    ("claude-fable-5", 10.0, 50.0),
    ("claude-mythos-5", 10.0, 50.0),
    ("claude-opus-5", 5.0, 25.0),
    ("claude-opus-4-8", 5.0, 25.0),
    ("claude-opus-4-7", 5.0, 25.0),
    ("claude-sonnet-5", 3.0, 15.0),
    ("claude-sonnet-4-6", 3.0, 15.0),
    ("claude-haiku-4-5", 1.0, 5.0),
];
const DEFAULT_RATE: (f64, f64) = (5.0, 25.0);

fn rate_for(model: &str) -> (f64, f64) {
    // The [1m] context variant bills at the base model's rate.
    let base = model.strip_suffix("[1m]").unwrap_or(model);
    RATES
        .iter()
        .find(|(name, _, _)| *name == base)
        .map_or(DEFAULT_RATE, |(_, i, o)| (*i, *o))
}

fn message_cost(model: &str, u: &TokenUsage) -> f64 {
    let (input, output) = rate_for(model);
    (u.input_tokens as f64 * input
        + u.cache_creation_input_tokens as f64 * input * 1.25
        + u.cache_read_input_tokens as f64 * input * 0.1
        + u.output_tokens as f64 * output)
        / 1e6
}

/// What the cache remembers about one transcript file.
#[derive(Serialize, Deserialize, PartialEq)]
struct Cached {
    /// mtime (seconds since epoch) and size: a transcript only ever grows,
    /// so this pair changing is the same as "needs re-reading".
    m: u64,
    s: u64,
    c: f64,
}

/// Priced sum of this month's Bedrock-served messages, incremental per file.
fn bedrock_month_cost() -> f64 {
    let (month, month_start) = month_bounds();
    let dir = cswap::home().join(".cache/ccflip");
    let _ = fs::create_dir_all(&dir);
    // Only this month is interesting; drop every other month's cache.
    if let Ok(entries) = fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("bedrock-cost-") && !name.contains(&month) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    let cache_path = dir.join(format!("bedrock-cost-rs1-{month}.json"));
    let mut cache: HashMap<String, Cached> = fs::read_to_string(&cache_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();

    let mut files = Vec::new();
    for root in [
        cswap::home().join(".claude/projects"),
        cswap::data_home().join("claude-swap/sessions"),
    ] {
        collect_transcripts(&root, month_start, &mut files);
    }

    let mut total = 0.0;
    let mut changed = false;
    let mut seen: HashSet<String> = HashSet::new();
    for (path, stamp) in files {
        let key = path.display().to_string();
        seen.insert(key.clone());
        let cost = match cache.get(&key) {
            Some(hit) if hit.m == stamp.0 && hit.s == stamp.1 => hit.c,
            _ => {
                let cost = file_cost(&path);
                cache.insert(
                    key,
                    Cached {
                        m: stamp.0,
                        s: stamp.1,
                        c: cost,
                    },
                );
                changed = true;
                cost
            }
        };
        total += cost;
    }
    // Deleted transcripts must not keep occupying the cache forever.
    let before = cache.len();
    cache.retain(|k, _| seen.contains(k));
    if (changed || cache.len() != before)
        && let Ok(json) = serde_json::to_string(&cache)
    {
        let tmp = cache_path.with_extension("tmp");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, &cache_path);
        }
    }
    total
}

/// Every `*.jsonl` under `dir` touched this month, with its (mtime, size).
fn collect_transcripts(
    dir: &Path,
    cutoff: Option<SystemTime>,
    out: &mut Vec<(PathBuf, (u64, u64))>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_transcripts(&path, cutoff, out);
            continue;
        }
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if cutoff.is_some_and(|cut| mtime < cut) {
            continue;
        }
        let secs = mtime.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        out.push((path, (secs, meta.len())));
    }
}

/// Priced Bedrock usage in one transcript. Multi-block messages repeat the
/// same usage under the same message id, so the last one per id wins.
fn file_cost(path: &Path) -> f64 {
    let Ok(file) = fs::File::open(path) else {
        return 0.0;
    };
    let mut per_message: HashMap<String, (String, TokenUsage)> = HashMap::new();
    // Transcripts run to gigabytes: stream, and only JSON-parse lines that
    // can possibly match — the substring scan is orders faster.
    for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
        let Ok(line) = line else { break };
        if !line.contains("msg_bdrk_") {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<TranscriptLine>(&line) else {
            continue;
        };
        let Some(msg) = parsed.message else { continue };
        let Some(id) = msg.id.filter(|i| i.starts_with("msg_bdrk_")) else {
            continue;
        };
        per_message.insert(
            id,
            (msg.model.unwrap_or_default(), msg.usage.unwrap_or_default()),
        );
    }
    per_message
        .values()
        .map(|(model, u)| message_cost(model, u))
        .sum()
}

/// ("YYYY-MM", first of the month). std has no calendar, so ask date(1) —
/// this tool is Linux-only anyway (`who` reads /proc). No date binary means
/// counting everything rather than nothing.
fn month_bounds() -> (String, Option<SystemTime>) {
    let Ok(out) = Command::new("sh")
        .args(["-c", r#"date +%Y-%m; date -d "$(date +%Y-%m-01)" +%s"#])
        .output()
    else {
        return ("unknown".to_string(), None);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let month = lines.next().unwrap_or("unknown").trim().to_string();
    let start = lines
        .next()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
    (month, start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_per_model_with_cache_multipliers() {
        let u = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
            cache_read_input_tokens: 1_000_000,
        };
        // Opus: 1M in @$5 + 1M cache-write @$6.25 + 1M cache-read @$0.50 + 1M out @$25.
        assert_eq!(message_cost("claude-opus-5", &u), 36.75);
        // The [1m] variant bills at the base model's rate.
        assert_eq!(message_cost("claude-opus-5[1m]", &u), 36.75);
        // Haiku is cheaper; an unknown model is priced as Opus, never free.
        assert_eq!(message_cost("claude-haiku-4-5", &u), 1.0 + 1.25 + 0.1 + 5.0);
        assert_eq!(
            message_cost("claude-something-unreleased", &u),
            message_cost("claude-opus-5", &u)
        );
    }

    #[test]
    fn one_usage_per_message_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        // Same message id twice (a multi-block message repeats it) plus a
        // non-Bedrock message that must not be priced at all.
        std::fs::write(
            &path,
            concat!(
                r#"{"message":{"id":"msg_bdrk_1","model":"claude-opus-5","usage":{"output_tokens":1000000}}}"#,
                "\n",
                r#"{"message":{"id":"msg_bdrk_1","model":"claude-opus-5","usage":{"output_tokens":1000000}}}"#,
                "\n",
                r#"{"message":{"id":"msg_direct","model":"claude-opus-5","usage":{"output_tokens":9000000}}}"#,
                "\n",
                "not json\n",
            ),
        )
        .unwrap();
        assert_eq!(file_cost(&path), 25.0);
    }

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
