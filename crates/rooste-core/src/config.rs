//! The configuration file: settings and saved profiles, as TOML.
//!
//! The file lives in the platform config directory, or next to the
//! executable in portable mode (when a `rooste-ssh.toml` or `portable` file
//! exists there), so the app can run from a USB stick.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Profile;

const FILE_NAME: &str = "config.toml";
const PORTABLE_FILE_NAME: &str = "rooste-ssh.toml";
/// The portable file's name before 0.4.0, renamed when found.
const OLD_PORTABLE_FILE_NAME: &str = "rooste.toml";
const PORTABLE_MARKER: &str = "portable";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// Application settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub font_size: f32,
    pub scrollback_lines: usize,
    /// Copy selected text to the clipboard as soon as it is selected.
    pub copy_on_select: bool,
    pub theme: Theme,
    /// Ask before closing a tab with an active connection.
    pub confirm_close: bool,
    /// User name used when a profile leaves it empty.
    pub default_user: String,
    /// Reconnect automatically when a connection drops.
    pub auto_reconnect: bool,
    /// Folder for SFTP downloads; empty means `~/Downloads`.
    pub download_dir: String,
    /// The folder last chosen with "Download to…"; empty if none.
    pub last_download_to: String,
    /// Folder where every session's output is saved as text; empty means
    /// sessions are only logged when asked.
    pub log_dir: String,
    /// Offer the built-in agent's keys to rooste-ssh's own sessions.
    pub use_builtin_agent: bool,
    /// Ask GitHub for a newer release at startup.
    pub check_updates: bool,
    /// A release the user chose not to be told about again.
    pub skipped_version: String,
    /// Number of scrollback lines captured per pane when attaching via `tmux -CC`.
    pub tmux_scrollback_capture: usize,
    /// Seconds of output lag before `tmux -CC` pauses a pane (`pause-after` flow control).
    pub tmux_pause_after_secs: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: 14.0,
            scrollback_lines: 10_000,
            copy_on_select: true,
            theme: Theme::Dark,
            confirm_close: true,
            default_user: String::new(),
            auto_reconnect: true,
            download_dir: String::new(),
            last_download_to: String::new(),
            log_dir: String::new(),
            use_builtin_agent: false,
            check_updates: true,
            skipped_version: String::new(),
            tmux_scrollback_capture: 1_000,
            tmux_pause_after_secs: 5,
        }
    }
}

/// Everything persisted between runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub settings: Settings,
    pub profiles: Vec<Profile>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not a valid configuration file", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("could not write {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl Config {
    /// Load from `path`. A missing file yields the default configuration.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source: Box::new(source),
        })
    }

    /// Write to `path` atomically (temporary file, then rename), creating
    /// the directory if needed.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let write_error = |source| ConfigError::Write {
            path: path.to_owned(),
            source,
        };
        let text = toml::to_string_pretty(self).map_err(|e| write_error(io::Error::other(e)))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(write_error)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(write_error)?;
        std::fs::rename(&tmp, path).map_err(write_error)
    }

    pub fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// Insert or replace the profile with the same id.
    pub fn upsert_profile(&mut self, profile: Profile) {
        match self.profiles.iter_mut().find(|p| p.id == profile.id) {
            Some(existing) => *existing = profile,
            None => self.profiles.push(profile),
        }
    }

    pub fn remove_profile(&mut self, id: &str) -> Option<Profile> {
        let index = self.profiles.iter().position(|p| p.id == id)?;
        Some(self.profiles.remove(index))
    }

    /// Add imported profiles, skipping any whose host, port, user and name
    /// match an existing profile. Returns how many were added.
    pub fn merge_imported(&mut self, imported: Vec<Profile>) -> usize {
        let mut added = 0;
        for profile in imported {
            let duplicate = self.profiles.iter().any(|p| {
                p.host == profile.host
                    && p.effective_port() == profile.effective_port()
                    && p.user == profile.user
                    && p.display_name() == profile.display_name()
            });
            if !duplicate {
                self.profiles.push(profile);
                added += 1;
            }
        }
        added
    }

    /// Folder paths in use, sorted, including parents of nested folders.
    pub fn folders(&self) -> Vec<String> {
        let mut folders: Vec<String> = self
            .profiles
            .iter()
            .flat_map(|p| {
                let parts: Vec<&str> = p.folder.split('/').filter(|s| !s.is_empty()).collect();
                (1..=parts.len()).map(move |n| parts[..n].join("/"))
            })
            .collect();
        folders.sort();
        folders.dedup();
        folders
    }
}

/// `%APPDATA%\rooste-ssh` on Windows, `~/Library/Application Support/rooste-ssh`
/// on macOS, `$XDG_CONFIG_HOME/rooste-ssh` (or `~/.config/rooste-ssh`)
/// elsewhere.
fn platform_config_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::home_dir().map(|h| h.join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::home_dir().map(|h| h.join(".config")))
    };
    base.map(|b| b.join("rooste-ssh"))
}

/// Where the configuration file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLocation {
    pub path: PathBuf,
    /// Stored next to the executable.
    pub portable: bool,
}

impl ConfigLocation {
    /// Portable mode if the executable's directory holds `rooste-ssh.toml` or
    /// a `portable` marker; otherwise the platform config directory.
    pub fn detect() -> Self {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_owned));
        if let Some(dir) = exe_dir
            && let Some(location) = Self::portable_in(&dir)
        {
            return location;
        }
        let path = platform_config_dir()
            .map(|dir| dir.join(FILE_NAME))
            .unwrap_or_else(|| PathBuf::from(FILE_NAME));
        Self {
            path,
            portable: false,
        }
    }

    /// The portable location in `dir`, if portable mode is enabled there.
    /// A `rooste.toml` from before 0.4.0 is renamed to `rooste-ssh.toml`, or
    /// used as it is if it can't be renamed (e.g. on a read-only drive).
    pub fn portable_in(dir: &Path) -> Option<Self> {
        let mut path = dir.join(PORTABLE_FILE_NAME);
        let old = dir.join(OLD_PORTABLE_FILE_NAME);
        if !path.exists() && old.exists() && std::fs::rename(&old, &path).is_err() {
            path = old;
        }
        (path.exists() || dir.join(PORTABLE_MARKER).exists()).then_some(Self {
            path,
            portable: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Protocol;
    use proptest::prelude::*;

    fn profile(name: &str, folder: &str) -> Profile {
        Profile {
            name: name.into(),
            folder: folder.into(),
            host: format!("{name}.example.com"),
            ..Profile::default()
        }
    }

    #[test]
    fn missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load(&dir.path().join("none.toml")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        let mut config = Config::default();
        config.settings.font_size = 16.0;
        config.settings.theme = Theme::Light;
        config.upsert_profile(Profile {
            port: Some(2222),
            protocol: Protocol::Telnet,
            tags: vec!["a".into(), "b".into()],
            key_file: Some("/home/me/.ssh/id_ed25519".into()),
            proxy_jump: Some("bastion".into()),
            ..profile("db", "Work")
        });
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        assert!(!path.with_extension("toml.tmp").exists());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[settings]"));
        assert!(text.contains("[[profiles]]"));
        assert!(text.contains("protocol = \"telnet\""));
    }

    #[test]
    fn partial_files_use_defaults() {
        let config: Config = toml::from_str(
            "[settings]\nfont_size = 12.0\n\n[[profiles]]\nname = \"x\"\nhost = \"h\"\n",
        )
        .unwrap();
        assert_eq!(config.settings.font_size, 12.0);
        assert_eq!(config.settings.scrollback_lines, 10_000);
        assert_eq!(config.profiles[0].host, "h");
        assert!(config.profiles[0].use_agent);
        assert!(!config.profiles[0].id.is_empty());
    }

    #[test]
    fn invalid_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "settings = [").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn upsert_remove_and_folders() {
        let mut config = Config::default();
        let mut a = profile("a", "Work/Prod");
        config.upsert_profile(a.clone());
        config.upsert_profile(profile("b", "Home"));
        config.upsert_profile(profile("c", ""));
        a.host = "changed".into();
        config.upsert_profile(a.clone());
        assert_eq!(config.profiles.len(), 3);
        assert_eq!(config.profile(&a.id).unwrap().host, "changed");
        assert_eq!(config.folders(), ["Home", "Work", "Work/Prod"]);
        assert!(config.remove_profile(&a.id).is_some());
        assert!(config.remove_profile(&a.id).is_none());
    }

    #[test]
    fn merge_skips_duplicates() {
        let mut config = Config::default();
        config.upsert_profile(profile("a", ""));
        let added = config.merge_imported(vec![profile("a", "Imported"), profile("b", "")]);
        assert_eq!(added, 1);
        assert_eq!(config.profiles.len(), 2);
    }

    #[test]
    fn portable_mode_detection() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(ConfigLocation::portable_in(dir.path()), None);
        std::fs::write(dir.path().join("portable"), "").unwrap();
        let location = ConfigLocation::portable_in(dir.path()).unwrap();
        assert!(location.portable);
        assert_eq!(location.path, dir.path().join("rooste-ssh.toml"));
    }

    #[test]
    fn old_portable_file_is_renamed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rooste.toml"), "[settings]\n").unwrap();
        let location = ConfigLocation::portable_in(dir.path()).unwrap();
        assert_eq!(location.path, dir.path().join("rooste-ssh.toml"));
        assert_eq!(
            std::fs::read_to_string(&location.path).unwrap(),
            "[settings]\n"
        );
        assert!(!dir.path().join("rooste.toml").exists());
    }

    fn arb_profile() -> impl Strategy<Value = Profile> {
        (
            "\\PC{0,20}",
            "[a-z/]{0,12}",
            proptest::collection::vec("\\PC{0,8}", 0..3),
            "[a-z0-9.-]{0,20}",
            proptest::option::of(1u16..),
            "\\PC{0,10}",
            any::<(bool, bool, bool)>(),
            proptest::option::of("[a-z@.]{1,20}"),
        )
            .prop_map(
                |(name, folder, tags, host, port, user, flags, proxy_jump)| Profile {
                    name,
                    folder,
                    tags,
                    host,
                    port,
                    user,
                    use_agent: flags.0,
                    save_password: flags.1,
                    allow_legacy: flags.2,
                    proxy_jump,
                    ..Profile::default()
                },
            )
    }

    proptest! {
        #[test]
        fn any_config_round_trips_through_toml(
            profiles in proptest::collection::vec(arb_profile(), 0..5),
            font_size in 6.0f32..40.0,
        ) {
            let config = Config {
                settings: Settings { font_size, ..Settings::default() },
                profiles,
            };
            let text = toml::to_string_pretty(&config).unwrap();
            let back: Config = toml::from_str(&text).unwrap();
            prop_assert_eq!(back, config);
        }
    }
}
