//! `ccflip update` — replace this binary with the latest GitHub release.

use std::env;

use anyhow::Result;

pub fn run() -> Result<()> {
    // Owner/name come from Cargo.toml's `repository` — one source of truth.
    let mut segments = env!("CARGO_PKG_REPOSITORY").rsplit('/');
    let (name, owner) = (segments.next().unwrap(), segments.next().unwrap());
    let mut cfg = self_update::backends::github::Update::configure();
    cfg.repo_owner(owner)
        .repo_name(name)
        .bin_name("ccflip")
        // Releases ship one static musl build; a `cargo install` (gnu) binary
        // must still find it.
        .target(&format!("{}-unknown-linux-musl", env::consts::ARCH)[..])
        .current_version(self_update::cargo_crate_version!())
        .show_download_progress(true);
    // Optional: lifts GitHub's anonymous rate limit (and a private repo).
    if let Ok(token) = env::var("GITHUB_TOKEN").or_else(|_| env::var("GH_TOKEN")) {
        cfg.auth_token(&token);
    }
    let status = cfg.build()?.update()?;
    println!("ccflip is now {}", status.version());
    Ok(())
}
