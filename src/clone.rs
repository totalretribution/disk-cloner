use crate::backup::{choose_cloner, fsck_repair, part_number};
use crate::lsblk::{self, BlkDev};
use crate::restore::target_part;
use crate::util;
use anyhow::{bail, Result};
use clap::Args;
use std::io::Write;
use std::path::PathBuf;

#[derive(Args)]
pub struct CloneArgs {
    /// Source disk to copy FROM, e.g. /dev/sdc.
    pub source: PathBuf,

    /// Target disk to copy TO, e.g. /dev/sdd (ALL DATA WILL BE DESTROYED).
    pub target: PathBuf,

    /// Run fsck on each source partition before cloning. WRITES TO THE SOURCE.
    #[arg(long)]
    pub fsck: bool,

    /// Required to actually write. Without it, prints the plan and stops.
    #[arg(long)]
    pub yes: bool,

    /// Proceed even if source or target appears mounted (DANGEROUS).
    #[arg(long)]
    pub force: bool,

    /// Print the plan and exit without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(args: CloneArgs) -> Result<()> {
    if !args.dry_run {
        util::require_root()?;
        for t in ["lsblk", "sfdisk", "partprobe", "mkswap"] {
            if !util::have(t) {
                bail!("required tool '{}' not found on PATH", t);
            }
        }
        if !util::have("partclone.dd") {
            bail!("partclone not installed (need partclone.dd at minimum)");
        }
    }

    let src = lsblk::probe(&args.source)?;
    let dst = lsblk::probe(&args.target)?;
    for (d, role) in [(&src, "source"), (&dst, "target")] {
        if d.dtype != "disk" {
            bail!("{} {} is type '{}', not a whole disk", role, d.dev_path(), d.dtype);
        }
    }
    if src.dev_path() == dst.dev_path() {
        bail!("source and target are the same disk ({})", src.dev_path());
    }
    let src_size = src.size.unwrap_or(0);
    let dst_size = dst.size.unwrap_or(0);
    if dst_size < src_size {
        bail!(
            "target {} ({} bytes) is smaller than source {} ({} bytes); \
             the partition table will not fit",
            dst.dev_path(),
            dst_size,
            src.dev_path(),
            src_size
        );
    }
    if !args.force {
        util::assert_not_mounted(&args.source)?;
        util::assert_not_mounted(&args.target)?;
    }

    let parts: Vec<&BlkDev> = src.children.iter().filter(|c| c.dtype == "part").collect();
    if parts.is_empty() {
        bail!("no partitions on source {}", src.dev_path());
    }

    // Resolve cloners up front so the plan is accurate.
    let mut plan: Vec<(&BlkDev, String)> = Vec::new();
    for p in &parts {
        plan.push((p, choose_cloner(p.fstype.as_deref())?));
    }

    let target = dst.dev_path();
    println!("Clone {} ({} bytes) -> {} ({} bytes)", src.dev_path(), src_size, target, dst_size);
    println!("Partitions:");
    for (p, cloner) in &plan {
        let dest = target_part(&target, part_number(&p.name));
        let how = if cloner == "mkswap" {
            "mkswap".to_string()
        } else {
            format!("{} -b", cloner)
        };
        println!(
            "  {} fs={} -> {} via {}",
            p.dev_path(),
            p.fstype.as_deref().unwrap_or("(none)"),
            dest,
            how
        );
    }

    if args.dry_run {
        println!("\n[dry-run] nothing written.");
        return Ok(());
    }
    if !args.yes {
        println!("\nRe-run with --yes to perform the destructive clone.");
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

    // 1. Copy the partition table source -> target.
    println!("\nCopying partition table {} -> {}", src.dev_path(), target);
    util::run_pipeline(&format!(
        "sfdisk -d '{}' | sfdisk '{}'",
        esc(&src.dev_path()),
        esc(&target)
    ))?;
    util::run("partprobe", &[&target]).ok();

    // 2. Clone each partition device-to-device (used blocks only).
    let total = plan.len();
    for (i, (p, cloner)) in plan.iter().enumerate() {
        let spart = p.dev_path();
        let dpart = target_part(&target, part_number(&p.name));

        if cloner == "mkswap" {
            println!("\n[{}/{}] Recreating swap -> {}", i + 1, total, dpart);
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
            a.push(dpart.clone());
            let refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
            util::run("mkswap", &refs)?;
            continue;
        }

        if args.fsck {
            fsck_repair(p)?;
        }

        println!("\n[{}/{}] Cloning {} -> {} with {}", i + 1, total, spart, dpart, cloner);
        // partclone.dd: no mode flag, copies source->output directly.
        // fs binaries: -b = device-to-device clone (used blocks only).
        if cloner == "partclone.dd" {
            util::run(cloner, &["-s", &spart, "-o", &dpart])?;
        } else {
            util::run(cloner, &["-b", "-s", &spart, "-o", &dpart])?;
        }
    }

    println!("\nClone complete: {} -> {}", src.dev_path(), target);
    Ok(())
}

fn esc(s: &str) -> String {
    s.replace('\'', r"'\''")
}
