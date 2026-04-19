//! Filesystem layout for persistent state.
//!
//! We use the `directories` crate to pick a platform-appropriate config dir
//! (`~/.config/syncmesh` on Linux, `%APPDATA%\syncmesh\config` on Windows,
//! `~/Library/Application Support/syncmesh` on macOS) and keep a single file:
//!
//! - `identity.key` — the raw 32-byte Ed25519 secret
//!
//! The directory is created on first run; permissions follow the OS default
//! for the parent. The identity file itself is chmod 0600 on Unix (handled in
//! `syncmesh-net::identity`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub identity: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("", "", "syncmesh")
            .context("no valid home directory found for config")?;
        let config_dir = dirs.config_dir().to_path_buf();
        std::fs::create_dir_all(&config_dir)
            .with_context(|| format!("creating config dir {}", config_dir.display()))?;
        let identity = config_dir.join("identity.key");
        Ok(Self {
            config_dir,
            identity,
        })
    }
}
