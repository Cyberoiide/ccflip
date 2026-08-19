//! Live claude processes from /proc — shared by `ccflip who` and the
//! watcher's mid-session tier switching.
//!
//! ponytail: /proc scan, Linux only; matches the native-installer binary
//! (exe under .../claude/versions/) and anything literally named claude.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub struct ClaudeProc {
    pub pid: i32,
    pub cwd: String,
    /// `CLAUDE_CONFIG_DIR` at spawn time: a pinned cswap session profile.
    pub config_dir: Option<String>,
    /// `CLAUDE_CODE_USE_BEDROCK` set to something non-empty at spawn time.
    /// (`=` or `=0` are opt-outs — the statusline reads it the same way.)
    pub bedrock: bool,
    /// `<parent exe basename>:<parent comm>` — how a harness-spawned process
    /// is told apart from a terminal one.
    pub parent_tag: String,
    pub age: Duration,
}

impl ClaudeProc {
    /// SDK harnesses respawn a killed process on the next message with the
    /// same session id; a terminal session would just die. Node renames its
    /// main thread ("MainThread"), so the parent's exe is checked too.
    pub fn harness_spawned(&self) -> bool {
        ["node", "bun", "t3code", "electron"]
            .iter()
            .any(|h| self.parent_tag.contains(h))
    }
}

pub fn claude_procs() -> Vec<ClaudeProc> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            let dir = entry.path();
            let exe = fs::read_link(dir.join("exe")).ok()?;
            let is_claude = exe.to_string_lossy().contains("/claude/versions/")
                || exe.file_name().is_some_and(|n| n == "claude");
            if !is_claude {
                return None;
            }
            let environ = fs::read(dir.join("environ")).ok()?;
            let mut bedrock = false;
            let mut config_dir = None;
            for var in environ.split(|b| *b == 0) {
                if let Some(v) = var.strip_prefix(b"CLAUDE_CODE_USE_BEDROCK=") {
                    bedrock = !v.is_empty();
                } else if let Some(v) = var.strip_prefix(b"CLAUDE_CONFIG_DIR=") {
                    config_dir.get_or_insert_with(|| String::from_utf8_lossy(v).into_owned());
                }
            }
            Some(ClaudeProc {
                pid,
                cwd: fs::read_link(dir.join("cwd"))
                    .map(|c| c.display().to_string())
                    .unwrap_or_default(),
                config_dir: config_dir.filter(|d| !d.is_empty()),
                bedrock,
                parent_tag: parent_tag(&dir),
                age: fs::metadata(&dir)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

fn parent_tag(proc_dir: &Path) -> String {
    let ppid = fs::read_to_string(proc_dir.join("status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("PPid:").map(|v| v.trim().to_string()))
        })
        .unwrap_or_default();
    let parent = PathBuf::from("/proc").join(&ppid);
    let exe = fs::read_link(parent.join("exe"))
        .ok()
        .and_then(|e| e.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let comm = fs::read_to_string(parent.join("comm")).unwrap_or_default();
    format!("{exe}:{}", comm.trim())
}
