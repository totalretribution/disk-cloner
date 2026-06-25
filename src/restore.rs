use crate::manifest::Manifest;
use crate::util;
use anyhow::{bail, Context, Result};
use clap::Args;
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

    // Final interactive confirmation in addition to --yes.
    print!("\nType the target device path to confirm erase [{}]: ", target);
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if line.trim() != target {
        bail!("confirmation mismatch; aborting.");
    }

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

        // Swap was never imaged — recreate it, preserving UUID/label so fstab matches.
        if p.cloner == "mkswap" {
            println!("\nRecreating swap #{} -> {}", p.number, tp);
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
            a.push(tp.clone());
            let refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
            util::run("mkswap", &refs)?;
            continue;
        }

        let img_name = p
            .image_file
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("partition #{} has no image_file", p.number))?;
        let img = args.image_dir.join(img_name);
        let img_str = img.to_str().context("image path not UTF-8")?;
        println!("\nRestoring #{} {} -> {}", p.number, img_name, tp);

        // partclone.dd has no restore-mode flag; the fs binaries need -r.
        let mode = if p.cloner == "partclone.dd" { "" } else { "-r " };
        let restore = format!("{} {}-s - -o '{}'", p.cloner, mode, esc(&tp));
        let script = match m.compression.as_str() {
            "zstd" => format!("zstd -dc '{}' | {}", esc(img_str), restore),
            "gzip" => format!("gzip -dc '{}' | {}", esc(img_str), restore),
            "none" => format!(
                "{} {}-s '{}' -o '{}'",
                p.cloner,
                mode,
                esc(img_str),
                esc(&tp)
            ),
            other => bail!("unknown compression '{}' in manifest", other),
        };
        util::run_pipeline(&script)?;
    }

    println!("\nRestore complete to {}", target);
    Ok(())
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

fn esc(s: &str) -> String {
    s.replace('\'', r"'\''")
}
