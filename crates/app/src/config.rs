//! User settings, stored as TOML in the OS config directory.

use std::path::PathBuf;

use bluesnap_platform::{Native, Platform};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Global shortcut, e.g. "Ctrl+Alt+A" or "Alt+Cmd+A".
    pub hotkey: String,
    /// Folder the save dialog opens in. Defaults to the Pictures folder.
    pub save_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self { hotkey: Native::DEFAULT_HOTKEY.to_string(), save_dir: None }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("bluesnap").join("config.toml"))
    }

    /// Reads the config file, creating a commented default one on first run.
    /// Problems are logged and fall back to defaults so the app always starts.
    pub fn load() -> Self {
        let Some(path) = Self::path() else { return Self::default() };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                log::error!("invalid config {}: {e}", path.display());
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| {
                    std::fs::write(&path, Self::default_file())
                });
                match written {
                    Ok(()) => log::info!("wrote default config to {}", path.display()),
                    Err(e) => log::warn!("could not write {}: {e}", path.display()),
                }
                Self::default()
            }
            Err(e) => {
                log::error!("could not read {}: {e}", path.display());
                Self::default()
            }
        }
    }

    fn default_file() -> String {
        format!(
            "# bluesnap settings. Restart bluesnap after editing.\n\
             \n\
             # Global shortcut that starts a capture. Modifiers: Ctrl, Alt (Option), Shift,\n\
             # Cmd (Super/Win). Keys: A-Z, 0-9, F1-F24, PrintScreen, ...\n\
             hotkey = \"{}\"\n\
             \n\
             # Folder the save dialog starts in (defaults to your Pictures folder).\n\
             # save_dir = \"/path/to/folder\"\n",
            Native::DEFAULT_HOTKEY
        )
    }

    pub fn save_dir(&self) -> PathBuf {
        self.save_dir
            .clone()
            .or_else(dirs::picture_dir)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_parses_to_defaults() {
        let parsed: Config = toml::from_str(&Config::default_file()).unwrap();
        assert_eq!(parsed.hotkey, Native::DEFAULT_HOTKEY);
        assert!(parsed.save_dir.is_none());
    }

    #[test]
    fn default_hotkey_is_valid() {
        assert!(Native::DEFAULT_HOTKEY.parse::<global_hotkey::hotkey::HotKey>().is_ok());
    }
}
