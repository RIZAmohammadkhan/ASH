use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKeySource {
    User,
    Embedded,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub openrouter_api_key: Option<String>,
    pub default_model: Option<String>,
    pub shell: Option<String>,
}

impl Config {
    pub fn load(path_override: Option<PathBuf>) -> Result<Self> {
        let path = config_path(path_override)?;

        if !path.exists() {
            write_default_config(&path)?;
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config at {}", path.display()))?;
        let config = toml::from_str::<Self>(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;

        Ok(config)
    }

    pub fn effective_api_key(&self) -> Option<String> {
        self.trimmed_user_api_key()
            .map(ToOwned::to_owned)
            .or_else(|| option_env!("ASH_EMBEDDED_OPENROUTER_KEY").map(ToOwned::to_owned))
    }

    pub fn api_key_source(&self) -> Option<ApiKeySource> {
        if self.trimmed_user_api_key().is_some() {
            Some(ApiKeySource::User)
        } else if option_env!("ASH_EMBEDDED_OPENROUTER_KEY").is_some() {
            Some(ApiKeySource::Embedded)
        } else {
            None
        }
    }

    pub fn has_user_api_key(&self) -> bool {
        self.trimmed_user_api_key().is_some()
    }

    pub fn default_shell(&self) -> String {
        self.shell
            .as_deref()
            .filter(|shell| !shell.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(detect_default_shell)
    }

    fn trimmed_user_api_key(&self) -> Option<&str> {
        self.openrouter_api_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

pub fn config_dir() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|dir| dir.join("ash"))
        .context("could not resolve a config directory")
}

pub fn config_path(path_override: Option<PathBuf>) -> Result<PathBuf> {
    match path_override {
        Some(path) => Ok(path),
        None => Ok(config_dir()?.join("config.toml")),
    }
}

pub fn models_cache_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("models_cache.json"))
}

fn detect_default_shell() -> String {
    if cfg!(windows) {
        env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    } else {
        env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

fn write_default_config(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }

    let default_shell = detect_default_shell();
    let template = format!(
        "# ash configuration\n\
         # Add your OpenRouter key here to unlock paid models.\n\
         # openrouter_api_key = \"sk-or-v1-...\"\n\
         \n\
         # Optional preferred model id.\n\
         # default_model = \"openrouter/auto\"\n\
         \n\
         # Optional shell override.\n\
         shell = \"{default_shell}\"\n"
    );

    fs::write(path, template)
        .with_context(|| format!("failed to write default config to {}", path.display()))
}
