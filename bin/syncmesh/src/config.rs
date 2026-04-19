//! Filesystem layout for persistent state + on-disk `Config`.
//!
//! Two concerns live here:
//!
//! 1. [`Paths`] — where syncmesh keeps its config file and identity. We use
//!    the `directories` crate to pick a platform-appropriate dir
//!    (`~/.config/syncmesh` on Linux, `%APPDATA%\syncmesh\config` on Windows,
//!    `~/Library/Application Support/syncmesh` on macOS).
//! 2. [`Config`] — the TOML schema users edit to change defaults. CLI flags
//!    override config values; config values override built-in defaults.
//!
//! On first run, if no `config.toml` exists, [`Config::load_or_write_template`]
//! drops a fully-commented template next to the identity file so the user can
//! discover it without reading docs.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub identity: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("", "", "syncmesh")
            .context("no valid home directory found for config")?;
        let config_dir = dirs.config_dir().to_path_buf();
        std::fs::create_dir_all(&config_dir)
            .with_context(|| format!("creating config dir {}", config_dir.display()))?;
        let config_file = config_dir.join("config.toml");
        let identity = config_dir.join("identity.key");
        Ok(Self {
            config_dir,
            config_file,
            identity,
        })
    }
}

/// How syncmesh should obtain the mpv handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MpvSpawn {
    /// Spawn our own mpv and talk to its IPC socket.
    #[default]
    Auto,
    /// Don't spawn; connect to an IPC socket opened by a Lua-script mpv.
    Script,
    /// Don't spawn or connect — useful for headless smoke tests.
    Disabled,
}

/// The user-editable subset of syncmesh configuration.
///
/// Every field is `Option`-ish in spirit: the file only overrides what the
/// user actually sets, and CLI flags still take precedence. Defaults for
/// unset fields are baked into the code, *not* into the written template.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub nickname: Option<String>,
    pub mpv_binary: Option<PathBuf>,
    pub mpv_spawn: Option<MpvSpawn>,
    pub override_mode: Option<bool>,
    /// `None` or empty string = use n0 default relays.
    pub relay: Option<String>,
    /// `error`|`warn`|`info`|`debug`|`trace`. Free-form, validated at tracing-init time.
    pub log_level: Option<String>,
    /// Relative paths are resolved against the config dir.
    pub identity_path: Option<PathBuf>,
    /// Render a QR code for the ticket when creating a room.
    pub qr: Option<bool>,
}

/// Template written to `config.toml` on first run. Every line is commented
/// out so the shipped defaults remain in force until the user un-comments
/// and edits.
const TEMPLATE: &str = "\
# syncmesh config. Every line here is a default — uncomment and edit to override.
# CLI flags beat config; config beats the built-in defaults.

# nickname      = \"divyam\"                        # your peer nickname
# mpv_binary    = \"/usr/bin/mpv\"                  # override mpv lookup
# mpv_spawn     = \"auto\"                          # \"auto\" | \"script\" | \"disabled\"
# override_mode = false                             # default state of ready-gate override
# relay         = \"https://relay.example.com\"     # optional — empty = use n0 defaults
# log_level     = \"info\"                          # error | warn | info | debug | trace
# identity_path = \"identity.key\"                  # rel to config_dir; absolute also allowed
# qr            = false                             # print a QR code under the ticket on create
";

impl Config {
    /// Load `config.toml` from `path`; if it's missing, drop the template
    /// next to it and return defaults. I/O errors (not file-missing) bubble.
    pub fn load_or_write_template(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(body) => toml::from_str(&body)
                .with_context(|| format!("parsing config at {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::write(path, TEMPLATE)
                    .with_context(|| format!("writing config template to {}", path.display()))?;
                Ok(Self::default())
            }
            Err(e) => Err(e).with_context(|| format!("reading config at {}", path.display())),
        }
    }

    /// Resolve `identity_path`, defaulting to `<config_dir>/identity.key` and
    /// treating relative overrides as relative to `config_dir`.
    pub fn identity_path(&self, paths: &Paths) -> PathBuf {
        match self.identity_path.as_ref() {
            None => paths.identity.clone(),
            Some(p) if p.is_absolute() => p.clone(),
            Some(p) => paths.config_dir.join(p),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_writes_template_and_returns_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let cfg = Config::load_or_write_template(&path).unwrap();
        assert!(path.exists(), "template was not written");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# nickname"));
        // All defaults are None.
        assert!(cfg.nickname.is_none());
        assert!(cfg.mpv_spawn.is_none());
        assert!(cfg.relay.is_none());
    }

    #[test]
    fn parses_populated_toml() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "nickname = \"d\"\nmpv_spawn = \"script\"\nrelay = \"https://r.example\"\nqr = true\n",
        )
        .unwrap();
        let cfg = Config::load_or_write_template(&path).unwrap();
        assert_eq!(cfg.nickname.as_deref(), Some("d"));
        assert_eq!(cfg.mpv_spawn, Some(MpvSpawn::Script));
        assert_eq!(cfg.relay.as_deref(), Some("https://r.example"));
        assert_eq!(cfg.qr, Some(true));
    }

    #[test]
    fn malformed_toml_surfaces_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "nickname = [1, 2]\n").unwrap();
        let err = Config::load_or_write_template(&path).unwrap_err();
        assert!(format!("{err:#}").contains("parsing config"));
    }

    #[test]
    fn identity_path_defaults_and_resolves_relative() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths {
            config_dir: tmp.path().to_path_buf(),
            config_file: tmp.path().join("config.toml"),
            identity: tmp.path().join("identity.key"),
        };

        let default = Config::default().identity_path(&paths);
        assert_eq!(default, paths.identity);

        let rel = Config {
            identity_path: Some(PathBuf::from("other.key")),
            ..Config::default()
        }
        .identity_path(&paths);
        assert_eq!(rel, tmp.path().join("other.key"));

        #[cfg(unix)]
        {
            let abs = Config {
                identity_path: Some(PathBuf::from("/tmp/abs.key")),
                ..Config::default()
            }
            .identity_path(&paths);
            assert_eq!(abs, PathBuf::from("/tmp/abs.key"));
        }
    }
}
