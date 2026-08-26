use crate::manifest::{Manifest, PartEntry};
use crate::shrink::{self, ShrinkOpts};
use crate::util;
use anyhow::{bail, Context, Result};
use clap::Args;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

#[derive(Args)]
pub struct RestoreArgs {
    /// Image directory containing manifest.json.
    pub image_dir: PathBuf,

    /// Target disk to overwrite, e.g. /dev/sdb (ALL DATA WILL BE DESTROYED).
    pub device: PathBuf,

    /// Skip rewriting the partition table (restore into existing partitions).
    #[arg(long)]
    pub skip_ptable: bool,

    /// Fit the image onto a target disk of a different size by shrinking (or
    /// growing) the ext2/3/4 partitions and swap. Needs scratch space.
    #[arg(long)]
    pub shrink: bool,

    /// With --shrink: size for each swap partition (e.g. 4G, or 0 to drop it).
    /// Default: scale it by how much the disk shrank, floor 512 MiB.
    #[arg(long, value_name = "SIZE")]
    pub swap_size: Option<String>,

    /// With --shrink: directory for staging files. Needs room for the
    /// uncompressed used data of every ext partition.
    #[arg(long, default_value = "/var/tmp", value_name = "DIR")]
    pub scratch: PathBuf,

    /// With --shrink: keep the staging files and loop devices for inspection.
    #[arg(long)]
    pub keep_scratch: bool,

    /// With --shrink: pin one partition to an exact size, e.g. --part-size 2=100G.
    /// Repeatable. Overrides the automatic share-out for that partition.
    #[arg(long, value_name = "N=SIZE")]
    pub part_size: Vec<String>,

    /// With --shrink: print the projected layout and stop. Stages nothing,
    /// writes nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Required to actually write. Without it, restore prints the plan and stops.
    #[arg(long)]
    pub yes: bool,

    /// Proceed even if the target appears mounted (DANGEROUS).
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: RestoreArgs) -> Result<()> {
    util::require_root()?;
    for t in ["sfdisk", "partprobe"] {
        if !util::have(t) {
            bail!("required tool '{}' not found on PATH", t);
        }
    }

    let m = Manifest::load(&args.image_dir)?;
    let target = args
        .device
        .to_str()
        .context("target path not UTF-8")?
        .to_string();

    if !args.force {
        util::assert_not_mounted(&args.device)?;
    }

    if args.shrink {
        if args.skip_ptable {
            bail!("--shrink rewrites the partition table; it cannot be combined with --skip-ptable");
        }
        let opts = ShrinkOpts {
            swap_size: args
                .swap_size
                .as_deref()
                .map(util::parse_size)
                .transpose()?,
            part_size: parse_part_sizes(&args.part_size)?,
            scratch: args.scratch.clone(),
            keep_scratch: args.keep_scratch,
            dry_run: args.dry_run,
            yes: args.yes,
        };
        return shrink::run(&args.image_dir, &target, &m, &opts);
    }

    // Straight restore: the saved layout is replayed verbatim, so the target
    // must be at least as large as the source disk.
    let target_size = util::device_size_bytes(&target)?;
    if !args.skip_ptable && target_size < m.disk_size_bytes {
        bail!(
            "target {} is {} but the image was taken from a {} disk.\n\
             The saved partition table cannot be replayed onto a smaller disk.\n\
             Re-run with --shrink to refit the layout.",
            target,
            util::human(target_size),
            util::human(m.disk_size_bytes)
        );
    }

    println!("Restore image from : {}", args.image_dir.display());
    println!("  created : {}", m.created_utc);
    println!("  source  : {} ({} bytes)", m.source_device, m.disk_size_bytes);
    println!("  compression: {}", m.compression);
    println!("Target disk (WILL BE ERASED): {}", target);
    println!("Partitions to restore:");
    for p in &m.partitions {
        let tp = target_part(&target, p.number);
        let what = p.image_file.as_deref().unwrap_or("(swap: mkswap)");
        println!(
            "  #{} {} ({}) {} bytes -> {}",
            p.number, what, p.cloner, p.size_bytes, tp
        );
    }

    if !args.yes {
        println!("\nRe-run with --yes to perform the destructive restore.");
        return Ok(());
    }

    confirm_target(&target)?;

    // 1. Partition table.
    if !args.skip_ptable {
        let ptable = args.image_dir.join(&m.ptable_file);
        let ptable_str = ptable.to_str().context("ptable path not UTF-8")?;
        println!("\nRestoring partition table to {}", target);
        util::run_pipeline(&format!(
            "sfdisk '{}' < '{}'",
            esc(&target),
            esc(ptable_str)
        ))?;
        util::run("partprobe", &[&target]).ok();
    }

    // 2. Each partition.
    for p in &m.partitions {
        let tp = target_part(&target, p.number);
        if p.cloner == "mkswap" {
            println!("\nRecreating swap #{} -> {}", p.number, tp);
            make_swap(p, &tp)?;
            continue;
        }
        println!(
            "\nRestoring #{} {} -> {}",
            p.number,
            p.image_file.as_deref().unwrap_or("?"),
            tp
        );
        restore_partition(&args.image_dir, p, &m.compression, &tp)?;
    }

    println!("\nRestore complete to {}", target);
    Ok(())
}

/// Parse repeated `--part-size N=SIZE` values into partition number -> bytes.
fn parse_part_sizes(raw: &[String]) -> Result<BTreeMap<u32, u64>> {
    let mut out = BTreeMap::new();
    for item in raw {
        let (n, size) = item
            .split_once('=')
            .with_context(|| format!("--part-size expects N=SIZE, got '{}'", item))?;
        let number: u32 = n
            .trim()
            .parse()
            .with_context(|| format!("--part-size: '{}' is not a partition number", n))?;
        let bytes = util::parse_size(size)?;
        if out.insert(number, bytes).is_some() {
            bail!("--part-size given twice for partition {}", number);
        }
    }
    Ok(out)
}

/// Interactive last-chance confirmation: the user must retype the device path.
pub fn confirm_target(target: &str) -> Result<()> {
    print!("\nType the target device path to confirm erase [{}]: ", target);
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if line.trim() != target {
        bail!("confirmation mismatch; aborting.");
    }
    Ok(())
}

/// Recreate a swap area, preserving UUID/label so fstab keeps matching.
pub fn make_swap(p: &PartEntry, target: &str) -> Result<()> {
    let mut a: Vec<String> = Vec::new();
    if let Some(u) = &p.uuid {
        a.push("-U".into());
        a.push(u.clone());
    }
    if let Some(l) = &p.label {
        if !l.is_empty() {
            a.push("-L".into());
            a.push(l.clone());
        }
    }
    a.push(target.to_string());
    let refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    util::run("mkswap", &refs)
}

/// Decompress one partition image and hand it to partclone, writing `target`
/// (a partition device or a loop device).
pub fn restore_partition(
    image_dir: &std::path::Path,
    p: &PartEntry,
    compression: &str,
    target: &str,
) -> Result<()> {
    let img_name = p
        .image_file
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("partition #{} has no image_file", p.number))?;
    let img = image_dir.join(img_name);
    let img_str = img.to_str().context("image path not UTF-8")?;

    // partclone.dd has no restore-mode flag; the fs binaries need -r.
    let mode = if p.cloner == "partclone.dd" { "" } else { "-r " };
    let restore = format!("{} {}-s - -o '{}'", p.cloner, mode, esc(target));
    let script = match compression {
        "zstd" => format!("zstd -dc '{}' | {}", esc(img_str), restore),
        "gzip" => format!("gzip -dc '{}' | {}", esc(img_str), restore),
        "none" => format!(
            "{} {}-s '{}' -o '{}'",
            p.cloner,
            mode,
            esc(img_str),
            esc(target)
        ),
        other => bail!("unknown compression '{}' in manifest", other),
    };
    util::run_pipeline(&script)
}

/// Build a partition device path: /dev/sdb + 1 -> /dev/sdb1;
/// /dev/nvme0n1 + 1 -> /dev/nvme0n1p1 (suffix 'p' when name ends in a digit).
pub fn target_part(disk: &str, number: u32) -> String {
    let ends_digit = disk.chars().last().map(|c| c.is_ascii_digit()).unwrap_or(false);
    if ends_digit {
        format!("{}p{}", disk, number)
    } else {
        format!("{}{}", disk, number)
    }
}

pub fn esc(s: &str) -> String {
    s.replace('\'', r"'\''")
}
