use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub border: BorderConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BorderConfig {
    pub style: BorderStyle,
    pub active_color: String,
    pub inactive_color: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BorderStyle {
    Rounded,
    Heavy,
    Double,
    Single,
    Ascii,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            border: BorderConfig::default(),
        }
    }
}

impl Default for BorderConfig {
    fn default() -> Self {
        Self {
            style: BorderStyle::Rounded,
            active_color: "#61afef".to_string(),
            inactive_color: "#5c6370".to_string(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        match fs::read_to_string(&path) {
            Ok(content) => match toml::from_str(&content) {
                Ok(config) => config,
                Err(e) => {
                    eprintln!("tmux-pane-border: warning: failed to parse {}: {e}", path.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("tmux-pane-border")
        .join("config.toml")
}
