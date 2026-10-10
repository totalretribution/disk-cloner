//! Facts about drives for display, from the lsblk tree.

use eframe::egui::Color32;

use disk_cloner::lsblk::BlkDev;
use disk_cloner::util::human;

use crate::theme;

/// Whole drives worth offering: no empty card readers or zram.
pub fn candidates(disks: &[BlkDev]) -> impl Iterator<Item = &BlkDev> {
    disks.iter().filter(|d| {
        (d.dtype == "disk" || d.dtype == "loop")
            && d.size.unwrap_or(0) > 0
            && !d.name.starts_with("zram")
    })
}

pub fn find<'a>(disks: &'a [BlkDev], path: &str) -> Option<&'a BlkDev> {
    disks
        .iter()
        .find(|d| !path.is_empty() && d.dev_path() == path)
}

pub fn size(d: &BlkDev) -> u64 {
    d.size.unwrap_or(0)
}

/// A short human name: the model when lsblk knows it, else the kind of drive.
pub fn describe(d: &BlkDev) -> String {
    if let Some(m) = d.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        return m.to_string();
    }
    if d.dtype == "loop" {
        return "Loop device".into();
    }
    match (d.tran.as_deref(), d.rota) {
        (Some("usb"), _) => "USB drive".into(),
        (Some("nvme"), _) => "NVMe SSD".into(),
        (Some("mmc"), _) => "Memory card".into(),
        (_, Some(true)) => "Hard drive".into(),
        (Some("sata"), Some(false)) => "SATA SSD".into(),
        _ => "Drive".into(),
    }
}

/// Every mount point on the device or anything below it (partitions, LVM, LUKS).
pub fn mounts(d: &BlkDev) -> Vec<String> {
    let mut out = mounts_here(d);
    for c in &d.children {
        out.extend(mounts(c));
    }
    out
}

/// Mount points of this node only, not its children.
fn mounts_here(d: &BlkDev) -> Vec<String> {
    let mut out: Vec<String> = d
        .mountpoints
        .iter()
        .flatten()
        .chain(d.mountpoint.iter())
        .filter(|m| !m.is_empty())
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Holds the running system, so it must never be a target.
pub fn is_system(d: &BlkDev) -> bool {
    mounts(d)
        .iter()
        .any(|m| m == "/" || m == "/boot" || m == "/usr")
}

pub fn partitions(d: &BlkDev) -> impl Iterator<Item = &BlkDev> {
    d.children.iter().filter(|c| c.dtype == "part")
}

/// Segments for a partition bar of the whole drive.
pub fn bar_segments(d: &BlkDev) -> Vec<(u64, Color32)> {
    partitions(d)
        .map(|p| (size(p), theme::fs_color(p.fstype.as_deref())))
        .collect()
}

/// "fstype "label"" for one partition, or just the type.
pub fn fs_label(p: &BlkDev) -> String {
    let fs = p.fstype.as_deref().unwrap_or("unformatted");
    match p.label.as_deref().filter(|l| !l.is_empty()) {
        Some(l) => format!("{} \"{}\"", fs, l),
        None => fs.to_string(),
    }
}

/// What is on a drive now, for the target card.
pub fn holds_now(d: &BlkDev) -> String {
    let parts: Vec<&BlkDev> = partitions(d).collect();
    match parts.len() {
        0 => match d.fstype.as_deref() {
            Some(fs) => format!("Holds now: {} on the whole drive", fs),
            None => "Holds now: no partitions".into(),
        },
        n => {
            let mut list: Vec<String> = parts.iter().take(3).map(|p| fs_label(p)).collect();
            if n > 3 {
                list.push("…".into());
            }
            format!(
                "Holds now: {} partition{}, {}",
                n,
                if n == 1 { "" } else { "s" },
                list.join(", ")
            )
        }
    }
}

/// "/dev/sdb · 111.8 GiB"
pub fn path_and_size(d: &BlkDev) -> String {
    format!("{} · {}", d.dev_path(), human(size(d)))
}
