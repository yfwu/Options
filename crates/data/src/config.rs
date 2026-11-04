//! User configuration (`config.toml`).
//!
//! Location: `$OPTLAB_HOME/config.toml` when `OPTLAB_HOME` is set (portable mode),
//! else `<config_dir>/optlab/config.toml` (`~/.config/optlab`, `%APPDATA%\optlab`,
//! `~/Library/Application Support/optlab`). Environment overrides applied by
//! [`Config::load`]: `TRADIER_TOKEN`, `TRADIER_SANDBOX`, `POLYGON_API_KEY`
//! (or `MASSIVE_API_KEY`), `OPTLAB_RISK_FREE_RATE`.

use crate::Provider;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Tradier access token (production or sandbox).
    pub tradier_token: Option<String>,
    /// Use `sandbox.tradier.com` (delayed data, free developer account).
    pub tradier_sandbox: bool,
    /// Polygon.io / Massive API key.
    pub polygon_api_key: Option<String>,
    /// Provider tried first by `fetch_chain_auto` (None = CBOE first).
    pub default_provider: Option<Provider>,
    /// Manual risk-free rate override as a decimal (0.04 = 4%). None = latest 3M T-bill.
    pub risk_free_rate: Option<f64>,
    /// Override of the data/snapshot directory.
    pub data_dir: Option<PathBuf>,
    /// Limit the number of expiries fetched from per-expiry providers (Yahoo, Tradier). None = all.
    pub max_expiries: Option<usize>,
}

impl Config {
    /// Default config file path.
    pub fn default_path() -> Result<PathBuf> {
        if let Some(h) = std::env::var_os(crate::store::HOME_ENV).filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(h).join("config.toml"));
        }
        dirs::config_dir()
            .map(|d| d.join("optlab").join("config.toml"))
            .ok_or_else(|| anyhow!("cannot determine a config directory; set {}", crate::store::HOME_ENV))
    }

    /// Load from the default path (missing file = defaults) and apply env overrides.
    pub fn load() -> Result<Self> {
        let mut c = Self::load_file(&Self::default_path()?)?;
        c.apply_env();
        Ok(c)
    }

    /// Load a TOML file without env overrides; a missing file yields defaults.
    pub fn load_file(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("invalid config {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Apply environment-variable overrides.
    pub fn apply_env(&mut self) {
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        if let Some(t) = var("TRADIER_TOKEN") {
            self.tradier_token = Some(t);
        }
        if let Some(s) = var("TRADIER_SANDBOX") {
            self.tradier_sandbox = matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(k) = var("POLYGON_API_KEY").or_else(|| var("MASSIVE_API_KEY")) {
            self.polygon_api_key = Some(k);
        }
        if let Some(r) = var("OPTLAB_RISK_FREE_RATE").and_then(|r| r.parse::<f64>().ok()) {
            self.risk_free_rate = Some(r);
        }
    }

    /// Save to the default path (creating directories). Note: values that came from
    /// environment overrides are written too.
    pub fn save(&self) -> Result<PathBuf> {
        let p = Self::default_path()?;
        self.save_to(&p)?;
        Ok(p)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).with_context(|| format!("cannot create {}", d.display()))?;
        }
        let s = toml::to_string_pretty(self)?;
        std::fs::write(path, s).with_context(|| format!("cannot write {}", path.display()))
    }

    pub fn has_tradier(&self) -> bool {
        self.tradier_token.as_deref().is_some_and(|t| !t.trim().is_empty())
    }

    pub fn has_polygon(&self) -> bool {
        self.polygon_api_key.as_deref().is_some_and(|t| !t.trim().is_empty())
    }
}
