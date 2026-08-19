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
        .current_version(self_update::cargo_crate_version!())
        .show_download_progress(true);
    // Private repo: releases need an authenticated request.
    if let Ok(token) = env::var("GITHUB_TOKEN").or_else(|_| env::var("GH_TOKEN")) {
        cfg.auth_token(&token);
    }
    let status = cfg.build()?.update()?;
    println!("ccflip is now {}", status.version());
    Ok(())
}
