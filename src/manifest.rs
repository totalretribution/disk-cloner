use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Top-level metadata for one disk image.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub tool_version: String,
    pub created_utc: String,
    pub source_device: String,
    pub disk_size_bytes: u64,
    pub disk_model: Option<String>,
    /// Compression used for partition images ("zstd", "gzip", or "none").
    pub compression: String,
    /// File holding the `sfdisk -d` dump of the partition table.
    pub ptable_file: String,
    pub partitions: Vec<PartEntry>,
}

/// One imaged partition.
#[derive(Debug, Serialize, Deserialize)]
pub struct PartEntry {
    /// Source partition path at backup time, e.g. /dev/nvme0n1p2.
    pub source: String,
    /// 1-based partition number on the disk.
    pub number: u32,
    pub fstype: Option<String>,
    pub size_bytes: u64,
    pub label: Option<String>,
    pub uuid: Option<String>,
    /// partclone binary used, e.g. "partclone.ext4" or "partclone.dd".
    /// The special value "mkswap" means: not imaged, recreate with mkswap on restore.
    pub cloner: String,
    /// Image filename inside the directory. None for swap (recreated, not imaged).
    #[serde(default)]
    pub image_file: Option<String>,
}

impl Manifest {
    pub fn save(&self, dir: &Path) -> Result<()> {
        let p = dir.join("manifest.json");
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(&p, s).with_context(|| format!("writing {}", p.display()))?;
        Ok(())
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let p = dir.join("manifest.json");
        let s = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        serde_json::from_str(&s).context("parsing manifest.json")
    }
}
