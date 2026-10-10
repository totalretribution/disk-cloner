use crate::util;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::Path;

/// One node from `lsblk -J`. Disks have children (partitions).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // parttype/mountpoint captured for the manifest/future use
pub struct BlkDev {
    pub name: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(rename = "type", default)]
    pub dtype: String,
    #[serde(default)]
    pub fstype: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub parttype: Option<String>,
    #[serde(default)]
    pub mountpoint: Option<String>,
    /// Every mount point (a btrfs volume can be mounted several times);
    /// `[null]` when unmounted.
    #[serde(default)]
    pub mountpoints: Vec<Option<String>>,
    /// Hardware model string; disks only.
    #[serde(default)]
    pub model: Option<String>,
    /// Transport: "usb", "sata", "nvme", ... (`list_all` only).
    #[serde(default)]
    pub tran: Option<String>,
    /// Rotational (spinning) disk (`list_all` only).
    #[serde(default)]
    pub rota: Option<bool>,
    /// Logical sector size in bytes (`list_all` only).
    #[serde(rename = "log-sec", default)]
    pub log_sec: Option<u64>,
    #[serde(default)]
    pub children: Vec<BlkDev>,
}

#[derive(Debug, Deserialize)]
struct LsblkOut {
    blockdevices: Vec<BlkDev>,
}

const COLS: &str = "NAME,PATH,TYPE,FSTYPE,SIZE,LABEL,UUID,PARTTYPE,MOUNTPOINT,MODEL";

/// Columns for `list_all`: COLS plus MOUNTPOINTS, which needs util-linux 2.37+,
/// and a few descriptive ones. Kept out of `probe`, which the CLI relies on.
const LIST_COLS: &str =
    "NAME,PATH,TYPE,FSTYPE,SIZE,LABEL,UUID,PARTTYPE,MOUNTPOINT,MODEL,MOUNTPOINTS,TRAN,ROTA,LOG-SEC";

/// Probe a single device, returning its tree node.
pub fn probe(dev: &Path) -> Result<BlkDev> {
    let dev_str = dev.to_str().ok_or_else(|| anyhow!("device not UTF-8"))?;
    let json = util::capture("lsblk", &["-J", "-b", "-o", COLS, dev_str])
        .context("running lsblk on device")?;
    let parsed: LsblkOut = serde_json::from_str(&json).context("parsing lsblk JSON")?;
    parsed
        .blockdevices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("lsblk returned no device for {}", dev_str))
}

/// Every block device on the system, as a tree (disks with partition children).
pub fn list_all() -> Result<Vec<BlkDev>> {
    let json = util::capture("lsblk", &["-J", "-b", "-o", LIST_COLS]).context("running lsblk")?;
    let parsed: LsblkOut = serde_json::from_str(&json).context("parsing lsblk JSON")?;
    Ok(parsed.blockdevices)
}

impl BlkDev {
    /// Absolute device path, falling back to /dev/NAME.
    pub fn dev_path(&self) -> String {
        self.path
            .clone()
            .unwrap_or_else(|| format!("/dev/{}", self.name))
    }
}

/// `disk-cloner list`: human-readable tree of disks.
pub fn print_list() -> Result<()> {
    let out = util::capture("lsblk", &["-o", COLS])?;
    print!("{}", out);
    Ok(())
}
