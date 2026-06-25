use crate::lsblk::{self, BlkDev};
use crate::manifest::{Manifest, PartEntry};
use crate::util;
use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct BackupArgs {
    /// Source disk, e.g. /dev/nvme0n1 or /dev/sda (the whole disk, not a partition).
    pub device: PathBuf,

    /// Output directory for the image (created if missing).
    #[arg(short, long)]
    pub output: PathBuf,

    /// Compression for partition images.
    #[arg(short, long, value_enum, default_value_t = Compression::Zstd)]
    pub compression: Compression,

    /// Proceed even if the device appears mounted (DANGEROUS, snapshot at own risk).
    #[arg(long)]
    pub force: bool,

    /// Run a filesystem repair (fsck) on each partition before imaging.
    /// WRITES TO THE SOURCE DISK. Fixes dirty filesystems that partclone refuses.
    #[arg(long)]
    pub fsck: bool,

    /// Overwrite an output directory that already contains image files.
    #[arg(long)]
    pub overwrite: bool,

    /// Print the plan and exit without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Copy, Clone, clap::ValueEnum)]
pub enum Compression {
    Zstd,
    Gzip,
    None,
}

impl Compression {
    fn name(self) -> &'static str {
        match self {
            Compression::Zstd => "zstd",
            Compression::Gzip => "gzip",
            Compression::None => "none",
        }
    }
    fn ext(self) -> &'static str {
        match self {
            Compression::Zstd => ".zst",
            Compression::Gzip => ".gz",
            Compression::None => "",
        }
    }
    fn tool(self) -> Option<&'static str> {
        match self {
            Compression::Zstd => Some("zstd"),
            Compression::Gzip => Some("gzip"),
            Compression::None => None,
        }
    }
}

pub fn run(args: BackupArgs) -> Result<()> {
    if !args.dry_run {
        util::require_root()?;
        preflight(&args)?;
    }

    let disk = lsblk::probe(&args.device)?;
    if disk.dtype != "disk" {
        bail!(
            "{} is type '{}', not a whole disk. Pass the disk (e.g. /dev/sda), not a partition.",
            disk.dev_path(),
            disk.dtype
        );
    }
    if !args.force {
        util::assert_not_mounted(&args.device)?;
    }

    let parts: Vec<&BlkDev> = disk
        .children
        .iter()
        .filter(|c| c.dtype == "part")
        .collect();
    if parts.is_empty() {
        bail!("no partitions found on {}", disk.dev_path());
    }

    // Resolve a cloner binary per partition up front so dry-run is accurate.
    let mut plan: Vec<(&BlkDev, String)> = Vec::new();
    for p in &parts {
        plan.push((p, choose_cloner(p.fstype.as_deref())?));
    }

    println!("Source : {} ({} bytes)", disk.dev_path(), disk.size.unwrap_or(0));
    println!("Output : {}", args.output.display());
    println!("Compress: {}", args.compression.name());
    println!("Partitions:");
    for (p, cloner) in &plan {
        let action = if cloner == "mkswap" {
            "skip swap (recreate on restore)".to_string()
        } else {
            cloner.clone()
        };
        println!(
            "  {} fs={} size={} -> {}",
            p.dev_path(),
            p.fstype.as_deref().unwrap_or("(none)"),
            p.size.unwrap_or(0),
            action
        );
    }

    if args.dry_run {
        println!("\n[dry-run] nothing written.");
        return Ok(());
    }

    std::fs::create_dir_all(&args.output)
        .with_context(|| format!("creating {}", args.output.display()))?;
    guard_output_dir(&args.output, args.overwrite)?;

    // 1. Save the partition table.
    let ptable_file = "ptable.sfdisk";
    save_ptable(&disk.dev_path(), &args.output.join(ptable_file))?;

    // 2. Image each partition (optionally repairing it first).
    let total = plan.len();
    let mut entries = Vec::new();
    for (i, (p, cloner)) in plan.iter().enumerate() {
        if cloner == "mkswap" {
            println!(
                "\n[{}/{}] Skipping swap {} — recreated with mkswap on restore",
                i + 1,
                total,
                p.dev_path()
            );
            entries.push(PartEntry {
                source: p.dev_path(),
                number: part_number(&p.name),
                fstype: p.fstype.clone(),
                size_bytes: p.size.unwrap_or(0),
                label: p.label.clone(),
                uuid: p.uuid.clone(),
                cloner: "mkswap".to_string(),
                image_file: None,
            });
            continue;
        }
        if args.fsck {
            fsck_repair(p)?;
        }
        let entry =
            image_partition(p, cloner, args.compression, &args.output, i + 1, total)?;
        entries.push(entry);
    }

    // 3. Write the manifest.
    let manifest = Manifest {
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        created_utc: chrono::Utc::now().to_rfc3339(),
        source_device: disk.dev_path(),
        disk_size_bytes: disk.size.unwrap_or(0),
        disk_model: None,
        compression: args.compression.name().to_string(),
        ptable_file: ptable_file.to_string(),
        partitions: entries,
    };
    manifest.save(&args.output)?;

    println!("\nDone. Image written to {}", args.output.display());
    Ok(())
}

/// Verify external tools exist before touching anything.
fn preflight(args: &BackupArgs) -> Result<()> {
    for t in ["lsblk", "sfdisk"] {
        if !util::have(t) {
            bail!("required tool '{}' not found on PATH", t);
        }
    }
    if !util::have("partclone.dd") {
        bail!("partclone not installed (need at least partclone.dd). Install the 'partclone' package.");
    }
    if let Some(t) = args.compression.tool() {
        if !util::have(t) {
            bail!("compression tool '{}' not found on PATH", t);
        }
    }
    Ok(())
}

/// Pick the best partclone binary for a filesystem, falling back to partclone.dd.
pub fn choose_cloner(fstype: Option<&str>) -> Result<String> {
    // Swap holds only volatile data; never imaged, recreated with mkswap.
    if fstype == Some("swap") {
        return Ok("mkswap".to_string());
    }
    let candidates: Vec<&str> = match fstype {
        Some("ext2") | Some("ext3") | Some("ext4") => vec!["partclone.ext4", "partclone.extfs"],
        Some("xfs") => vec!["partclone.xfs"],
        Some("btrfs") => vec!["partclone.btrfs"],
        Some("ntfs") => vec!["partclone.ntfs"],
        Some("vfat") => vec!["partclone.vfat", "partclone.fat32"],
        Some("exfat") => vec!["partclone.exfat"],
        Some("f2fs") => vec!["partclone.f2fs"],
        Some("hfsplus") => vec!["partclone.hfsp"],
        // swap, unformatted, or unknown -> raw block copy.
        _ => vec![],
    };
    for c in candidates {
        if util::have(c) {
            return Ok(c.to_string());
        }
    }
    // Always available per preflight.
    Ok("partclone.dd".to_string())
}

/// Repair a partition's filesystem in place before imaging. Picks the right
/// repair tool per fstype. Modifies the SOURCE disk. fsck exit codes 1/2 mean
/// "errors corrected" (success); >=4 is a real failure.
pub fn fsck_repair(p: &BlkDev) -> Result<()> {
    let dev = p.dev_path();
    let fs = p.fstype.as_deref();
    // (program, args-before-device). None => no repair tool for this fs.
    let (prog, pre): (&str, Vec<&str>) = match fs {
        Some("ext2") | Some("ext3") | Some("ext4") => ("e2fsck", vec!["-f", "-y"]),
        Some("vfat") => ("fsck.vfat", vec!["-a", "-w"]),
        Some("exfat") => ("fsck.exfat", vec!["-y"]),
        Some("ntfs") => ("ntfsfix", vec![]),
        Some("f2fs") => ("fsck.f2fs", vec!["-f"]),
        Some("xfs") => ("xfs_repair", vec![]),
        other => {
            println!(
                "  [fsck] skipping {} (no repair tool for fs '{}')",
                dev,
                other.unwrap_or("none")
            );
            return Ok(());
        }
    };
    if !util::have(prog) {
        println!("  [fsck] skipping {}: '{}' not installed", dev, prog);
        return Ok(());
    }

    println!("\n[fsck] repairing {} with {} (writes to source disk)", dev, prog);
    let mut args: Vec<&str> = pre;
    args.push(&dev);
    let status = std::process::Command::new(prog)
        .args(&args)
        .status()
        .with_context(|| format!("spawning {}", prog))?;
    // fsck convention: 0=clean, 1=errors corrected, 2=corrected+reboot. >=4 real failure.
    match status.code() {
        Some(0..=3) => Ok(()),
        Some(c) => bail!(
            "{} on {} failed (exit {}); filesystem may be unrepairable. Fix manually.",
            prog,
            dev,
            c
        ),
        None => bail!("{} on {} terminated by signal", prog, dev),
    }
}

/// Refuse to write into a directory that already holds an image, unless
/// --overwrite is given (then wipe the prior artifacts so no stale parts remain).
fn guard_output_dir(dir: &std::path::Path, overwrite: bool) -> Result<()> {
    let mut stale = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let name = e?.file_name().to_string_lossy().into_owned();
        if name == "manifest.json" || name == "ptable.sfdisk" || name.starts_with("part-") {
            stale.push(name);
        }
    }
    if stale.is_empty() {
        return Ok(());
    }
    if !overwrite {
        bail!(
            "output dir {} already contains image files ({}). \
             Use a fresh directory or pass --overwrite.",
            dir.display(),
            stale.join(", ")
        );
    }
    for name in &stale {
        std::fs::remove_file(dir.join(name)).ok();
    }
    println!("[overwrite] removed {} stale file(s)", stale.len());
    Ok(())
}

fn save_ptable(disk_dev: &str, out: &std::path::Path) -> Result<()> {
    let out_str = out.to_str().context("ptable path not UTF-8")?;
    println!("\nSaving partition table -> {}", out_str);
    util::run_pipeline(&format!(
        "sfdisk -d '{}' > '{}'",
        shell_escape(disk_dev),
        shell_escape(out_str)
    ))
}

fn image_partition(
    p: &BlkDev,
    cloner: &str,
    comp: Compression,
    out_dir: &std::path::Path,
    idx: usize,
    total: usize,
) -> Result<PartEntry> {
    let number = part_number(&p.name);
    let src = p.dev_path();
    let base = format!("part-{}.{}.img", number, cloner.replace("partclone.", ""));
    let fname = format!("{}{}", base, comp.ext());
    let out_path = out_dir.join(&fname);
    let out_str = out_path.to_str().context("image path not UTF-8")?;

    println!(
        "\n[{}/{}] Imaging {} ({} bytes) with {} -> {}",
        idx,
        total,
        src,
        p.size.unwrap_or(0),
        cloner,
        fname
    );

    // partclone reads the device and emits its image to stdout (-o -).
    // The fs-specific binaries need -c (clone mode); partclone.dd has no mode
    // flag (it just copies source -> output) and rejects -c.
    let mode = if cloner == "partclone.dd" { "" } else { "-c " };
    let clone = format!("{} {}-s '{}' -o -", cloner, mode, shell_escape(&src));
    let script = match comp {
        Compression::Zstd => {
            format!("{} | zstd -T0 -q -f -o '{}'", clone, shell_escape(out_str))
        }
        Compression::Gzip => {
            format!("{} | gzip -c > '{}'", clone, shell_escape(out_str))
        }
        Compression::None => {
            // No pipe: write straight to the file with -O.
            format!(
                "{} {}-s '{}' -O '{}'",
                cloner,
                mode,
                shell_escape(&src),
                shell_escape(out_str)
            )
        }
    };
    util::run_pipeline(&script)?;

    Ok(PartEntry {
        source: src,
        number,
        fstype: p.fstype.clone(),
        size_bytes: p.size.unwrap_or(0),
        label: p.label.clone(),
        uuid: p.uuid.clone(),
        cloner: cloner.to_string(),
        image_file: Some(fname),
    })
}

/// Extract trailing digits of a partition name (sda1 -> 1, nvme0n1p2 -> 2).
pub fn part_number(name: &str) -> u32 {
    let digits: String = name.chars().rev().take_while(|c| c.is_ascii_digit()).collect();
    digits.chars().rev().collect::<String>().parse().unwrap_or(0)
}

/// Minimal single-quote escaping for shell. Our paths are controlled, but be safe.
fn shell_escape(s: &str) -> String {
    s.replace('\'', r"'\''")
}
