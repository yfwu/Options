//! Local snapshot store and download cache.
//!
//! Layout under the root (`$OPTLAB_HOME`, else `<data_dir>/optlab`, e.g.
//! `~/.local/share/optlab`, `%APPDATA%\optlab`, `~/Library/Application Support/optlab`):
//!
//! ```text
//! chains/{SYM}/{YYYYmmdd-HHMMSS}-{source}.json   OptionChain snapshots
//! cache/{key}.json                               cached histories (TTL by file mtime)
//! ```

use crate::util::safe_name;
use anyhow::{anyhow, Context, Result};
use chrono::NaiveDateTime;
use optlab_core::OptionChain;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Environment variable overriding the data root (also used for config in portable mode).
pub const HOME_ENV: &str = "OPTLAB_HOME";

/// Metadata of one stored chain snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub symbol: String,
    pub as_of: NaiveDateTime,
    pub source: String,
    pub path: PathBuf,
}

/// File-system store rooted at a directory.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// Default data root: `$OPTLAB_HOME` or `dirs::data_dir()/optlab`.
pub fn default_root() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    dirs::data_dir()
        .map(|d| d.join("optlab"))
        .ok_or_else(|| anyhow!("cannot determine a data directory; set {HOME_ENV}"))
}

impl Store {
    /// Store at an explicit root (created lazily on first write).
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Store at [`default_root`].
    pub fn open_default() -> Result<Self> {
        Ok(Self::at(default_root()?))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn chains_dir(&self) -> PathBuf {
        self.root.join("chains")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Path a chain would be saved to.
    pub fn snapshot_path(&self, chain: &OptionChain) -> PathBuf {
        let sym = safe_name(&chain.underlying);
        let file = format!("{}-{}.json", chain.as_of.format("%Y%m%d-%H%M%S"), safe_name(&chain.source).to_ascii_lowercase());
        self.chains_dir().join(sym).join(file)
    }

    /// Save a chain snapshot as JSON; returns the file path.
    pub fn save_chain(&self, chain: &OptionChain) -> Result<PathBuf> {
        let p = self.snapshot_path(chain);
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let json = serde_json::to_vec(chain)?;
        write_atomic(&p, &json)?;
        Ok(p)
    }

    /// Load a chain snapshot from a path.
    pub fn load_chain(&self, path: impl AsRef<Path>) -> Result<OptionChain> {
        let p = path.as_ref();
        let bytes = fs::read(p).with_context(|| format!("cannot read snapshot {}", p.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("invalid snapshot JSON {}", p.display()))
    }

    /// List snapshots (oldest first). `symbol = None` lists all symbols.
    pub fn list_snapshots(&self, symbol: Option<&str>) -> Result<Vec<SnapshotInfo>> {
        let base = self.chains_dir();
        let dirs: Vec<PathBuf> = match symbol {
            Some(s) => vec![base.join(safe_name(&crate::providers::clean_symbol(s)))],
            None => match fs::read_dir(&base) {
                Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect(),
                Err(_) => vec![],
            },
        };
        let mut out = Vec::new();
        for d in dirs {
            let sym = d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let Ok(rd) = fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                // {YYYYmmdd-HHMMSS}-{source}
                if stem.len() < 17 {
                    continue;
                }
                let Ok(as_of) = NaiveDateTime::parse_from_str(&stem[..15], "%Y%m%d-%H%M%S") else { continue };
                let source = stem[15..].trim_start_matches('-').to_string();
                out.push(SnapshotInfo { symbol: sym.clone(), as_of, source, path: p });
            }
        }
        out.sort_by(|a, b| (a.as_of, &a.symbol, &a.source).cmp(&(b.as_of, &b.symbol, &b.source)));
        Ok(out)
    }

    /// Most recent snapshot for `symbol`, if any.
    pub fn latest_snapshot(&self, symbol: &str) -> Result<Option<OptionChain>> {
        match self.list_snapshots(Some(symbol))?.last() {
            Some(info) => Ok(Some(self.load_chain(&info.path)?)),
            None => Ok(None),
        }
    }

    /// Delete a snapshot file.
    pub fn delete_snapshot(&self, info: &SnapshotInfo) -> Result<()> {
        fs::remove_file(&info.path).with_context(|| format!("cannot delete {}", info.path.display()))
    }

    fn cache_path(&self, key: &str) -> PathBuf {
        self.cache_dir().join(format!("{}.json", safe_name(key)))
    }

    /// Store a value in the cache.
    pub fn cache_put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let p = self.cache_path(key);
        fs::create_dir_all(self.cache_dir())?;
        write_atomic(&p, &serde_json::to_vec(value)?)
    }

    /// Cached value if present and younger than `ttl` (any age when `ttl` is `None`).
    pub fn cache_get<T: DeserializeOwned>(&self, key: &str, ttl: Option<Duration>) -> Option<T> {
        let p = self.cache_path(key);
        let meta = fs::metadata(&p).ok()?;
        if let Some(ttl) = ttl {
            let age = SystemTime::now().duration_since(meta.modified().ok()?).unwrap_or_default();
            if age > ttl {
                return None;
            }
        }
        serde_json::from_slice(&fs::read(&p).ok()?).ok()
    }
}

fn write_atomic(p: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = p.with_extension("json.tmp");
    fs::write(&tmp, bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, p).with_context(|| format!("cannot move into {}", p.display()))
}
