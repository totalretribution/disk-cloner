use crate::manifest::{Manifest, PartEntry};
use crate::pcimage::{self, ImageInfo};
use crate::ptable::Ptable;
use crate::restore;
use crate::util;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct ShrinkOpts {
    /// Explicit size for every swap partition. `Some(0)` drops swap entirely.
    pub swap_size: Option<u64>,
    /// `--part-size N=SIZE`: pin one partition to an exact size, in bytes.
    pub part_size: BTreeMap<u32, u64>,
    pub scratch: PathBuf,
    pub keep_scratch: bool,
    /// Print the projected layout and stop before staging anything.
    pub dry_run: bool,
    pub yes: bool,
}

/// Safety margin over a filesystem's reported minimum size.
const MARGIN_FRACTION: u64 = 10; // +10%
const MARGIN_FIXED: u64 = 128 << 20; // +128 MiB
const EXT_FLOOR: u64 = 64 << 20;
/// Extra scratch headroom over the used-block total, for resize2fs churn.
const SCRATCH_SLOP_FRACTION: u64 = 10; // +10%
const SCRATCH_SLOP_FIXED: u64 = 64 << 20; // +64 MiB per filesystem

/// How a partition's size is decided when refitting onto a different disk.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// partclone can only restore this filesystem at its original size.
    Fixed,
    /// ext2/3/4: staged in a scratch file, resized, then copied.
    Ext,
    /// Recreated by mkswap, so any size works.
    Swap,
}

/// A scratch copy of one ext filesystem: a sparse file on a loop device.
/// Dropping it detaches the loop device and removes the file.
struct Staged {
    file: PathBuf,
    loopdev: String,
    keep: bool,
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.keep {
            println!(
                "[scratch] keeping {} on {} (detach with: losetup -d {})",
                self.file.display(),
                self.loopdev,
                self.loopdev
            );
            return;
        }
        let _ = util::run("losetup", &["-d", &self.loopdev]);
        let _ = std::fs::remove_file(&self.file);
    }
}

struct Slot {
    number: u32,
    kind: Kind,
    /// Index into `manifest.partitions`, if this partition was imaged.
    entry: Option<usize>,
    fstype: String,
    /// Header facts, for ext partitions whose image we could read.
    info: Option<ImageInfo>,
    /// Sizes and positions are in sectors of `Ptable::sector_size`.
    orig: u64,
    min: u64,
    alloc: u64,
    /// Set from `--part-size`; the partition gets exactly this many sectors.
    pin: Option<u64>,
    staged: Option<Staged>,
}

impl Slot {
    /// Bytes a sparse staging file for this partition will occupy.
    fn stage_bytes(&self, compressed: u64) -> u64 {
        match &self.info {
            Some(i) => i.used_bytes + i.used_bytes / SCRATCH_SLOP_FRACTION + SCRATCH_SLOP_FIXED,
            // Unreadable header: fall back to guessing from the compressed size.
            None => compressed.saturating_mul(4),
        }
    }
}

struct Geometry {
    ss: u64,
    grain: u64,
    first: u64,
    last: u64,
    total_sectors: u64,
    target_bytes: u64,
}

pub fn run(image_dir: &Path, target: &str, m: &Manifest, opts: &ShrinkOpts) -> Result<()> {
    for t in ["losetup", "blockdev", "e2fsck", "resize2fs", "tune2fs"] {
        if !util::have(t) {
            bail!("--shrink needs '{}' on PATH", t);
        }
    }

    let pt = Ptable::load(&image_dir.join(&m.ptable_file))?;
    let geo = geometry(&pt, target)?;
    let mut slots = build_slots(&pt, m, image_dir, opts, &geo)?;

    if let Some(0) = opts.swap_size {
        slots.retain(|s| {
            if s.kind == Kind::Swap {
                println!("[plan] dropping swap #{} (--swap-size 0)", s.number);
                false
            } else {
                true
            }
        });
    }
    validate_pins(&slots, opts)?;

    // One grain of alignment slack per partition is held back from the budget.
    let budget = (geo.last - geo.first + 1)
        .checked_sub(geo.grain * (slots.len() as u64 + 1))
        .with_context(|| format!("{} is far too small for this image", target))?;

    let fixed_total: u64 = slots
        .iter()
        .filter(|s| s.kind == Kind::Fixed)
        .map(|s| util::align_up(s.orig, geo.grain))
        .sum();
    if fixed_total > budget {
        bail!(
            "cannot fit: partitions partclone cannot resize need {} on their own, but \
             {} only offers {}.\nMust keep their size: {}",
            util::human(fixed_total * geo.ss),
            target,
            util::human(budget * geo.ss),
            fixed_list(&slots)
        );
    }

    // --- Stage 0: projected layout from image headers alone. No writes. ---
    let scratch_need = scratch_requirement(&slots, m, image_dir);
    let measurable = slots
        .iter()
        .all(|s| s.kind != Kind::Ext || s.info.is_some());
    println!("\n=== Projected refit (estimated from image headers) ===");
    print_scratch(opts, scratch_need)?;
    if measurable {
        for s in slots.iter_mut() {
            s.min = estimated_min(s, &geo);
        }
        validate_pin_minimums(&slots, &geo)?;
        allocate(&mut slots, budget, &geo, opts, m)?;
        let projected = layout(&pt, &slots, &geo)?;
        print_plan(image_dir, m, target, &geo, &slots, Some(&projected), budget, PlanMode::Projected);
    } else {
        // Without a header there is nothing to project from, but staging will
        // measure the real figure, so this is not fatal.
        print_plan(image_dir, m, target, &geo, &slots, None, budget, PlanMode::Unknown);
        println!(
            "\nNote: sizes cannot be projected for every partition, so they will be \n\
             decided after staging. Re-run without --dry-run to see the real layout."
        );
    }

    if opts.dry_run {
        println!("\n[dry-run] nothing staged, nothing written.");
        return Ok(());
    }

    // --- Stage 1: stage every ext image, measure the real minimum, re-plan. ---
    require_scratch(opts, scratch_need)?;
    stage_ext_partitions(image_dir, m, opts, &mut slots, &geo)?;
    for s in slots.iter_mut().filter(|s| s.kind == Kind::Ext) {
        let staged = s.staged.as_ref().expect("ext slot is staged");
        let min_bytes = fs_min_bytes(&staged.loopdev)?;
        let with_margin = min_bytes + min_bytes / MARGIN_FRACTION + MARGIN_FIXED;
        s.min = util::align_up(
            std::cmp::max(with_margin, EXT_FLOOR).div_ceil(geo.ss),
            geo.grain,
        );
        println!(
            "[stage] #{} minimum partition size {} (resize2fs floor {})",
            s.number,
            util::human(s.min * geo.ss),
            util::human(min_bytes)
        );
    }
    validate_pin_minimums(&slots, &geo)?;
    allocate(&mut slots, budget, &geo, opts, m)?;
    let new_pt = layout(&pt, &slots, &geo)?;

    println!("\n=== Final refit (measured) ===");
    print_plan(image_dir, m, target, &geo, &slots, Some(&new_pt), budget, PlanMode::Exact);

    if !opts.yes {
        println!("\nRe-run with --yes to perform the destructive restore.");
        return Ok(()); // staging files are cleaned up on the way out
    }
    restore::confirm_target(target)?;

    // Resize every staged filesystem before touching the target, so a resize
    // failure is still non-destructive.
    for s in slots.iter().filter(|s| s.kind == Kind::Ext) {
        let staged = s.staged.as_ref().expect("ext slot is staged");
        println!(
            "\n[resize] partition #{} -> {}",
            s.number,
            util::human(s.alloc * geo.ss)
        );
        resize_staged(staged, s.alloc * geo.ss, s.alloc)?;
    }

    write_ptable(&new_pt, target, opts)?;

    for s in &slots {
        let tp = restore::target_part(target, s.number);
        match s.kind {
            Kind::Swap => {
                let p = &m.partitions[s.entry.expect("swap slot comes from the manifest")];
                println!(
                    "\nRecreating swap #{} ({}) -> {}",
                    s.number,
                    util::human(s.alloc * geo.ss),
                    tp
                );
                restore::make_swap(p, &tp)?;
            }
            Kind::Ext => {
                let p = &m.partitions[s.entry.expect("ext slot comes from the manifest")];
                let staged = s.staged.as_ref().expect("ext slot is staged");
                println!("\nCopying resized #{} -> {}", s.number, tp);
                util::run(&p.cloner, &["-b", "-s", &staged.loopdev, "-o", &tp])?;
            }
            Kind::Fixed => match s.entry {
                Some(i) => {
                    let p = &m.partitions[i];
                    println!(
                        "\nRestoring #{} {} -> {}",
                        s.number,
                        p.image_file.as_deref().unwrap_or("?"),
                        tp
                    );
                    restore::restore_partition(image_dir, p, &m.compression, &tp)?;
                }
                None => println!("\nLeaving #{} empty (no image in this backup)", s.number),
            },
        }
    }

    println!(
        "\nRestore complete to {} (refitted to {})",
        target,
        util::human(geo.target_bytes)
    );
    Ok(())
}

fn geometry(pt: &Ptable, target: &str) -> Result<Geometry> {
    let ss = pt.sector_size;
    let target_ss = util::logical_sector_size(target)?;
    if target_ss != ss {
        bail!(
            "sector size mismatch: the image was taken with {}-byte sectors, {} uses {}. \
             Refitting across sector sizes is not supported.",
            ss,
            target,
            target_ss
        );
    }
    let target_bytes = util::device_size_bytes(target)?;
    let total_sectors = target_bytes / ss;
    let grain = std::cmp::max(1, (1 << 20) / ss); // 1 MiB alignment
    let first = util::align_up(std::cmp::max(pt.first_lba.unwrap_or(grain), grain), grain);
    let last = total_sectors
        .checked_sub(1 + pt.tail_reserve())
        .filter(|l| *l > first)
        .with_context(|| format!("{} is too small to hold any partition", target))?;
    Ok(Geometry {
        ss,
        grain,
        first,
        last,
        total_sectors,
        target_bytes,
    })
}

/// Pair each partition-table entry with its manifest entry, decide whether its
/// size is negotiable, and read the image header when there is one.
fn build_slots(
    pt: &Ptable,
    m: &Manifest,
    image_dir: &Path,
    opts: &ShrinkOpts,
    geo: &Geometry,
) -> Result<Vec<Slot>> {
    let mut parts = pt.parts.clone();
    parts.sort_by_key(|p| p.start);

    let mut slots = Vec::new();
    for p in &parts {
        let entry = m.partitions.iter().position(|e| e.number == p.number);
        let (kind, fstype) = match entry {
            Some(i) => {
                let e = &m.partitions[i];
                let fs = e.fstype.clone().unwrap_or_else(|| "unknown".to_string());
                let kind = if e.cloner == "mkswap" {
                    Kind::Swap
                } else if is_ext(e) {
                    Kind::Ext
                } else {
                    Kind::Fixed
                };
                (kind, fs)
            }
            None => (Kind::Fixed, "unimaged".to_string()),
        };
        let info = match (kind, entry) {
            (Kind::Ext, Some(i)) => {
                let f = m.partitions[i]
                    .image_file
                    .as_deref()
                    .context("ext partition has no image_file")?;
                pcimage::probe(&image_dir.join(f), &m.compression)?
            }
            _ => None,
        };
        if kind == Kind::Ext && info.is_none() {
            println!(
                "[warn] could not read the partclone header of partition #{}; \
                 scratch space will be estimated from the compressed size",
                p.number
            );
        }
        slots.push(Slot {
            number: p.number,
            kind,
            entry,
            fstype,
            info,
            orig: p.size,
            min: p.size,
            alloc: p.size,
            pin: opts
                .part_size
                .get(&p.number)
                .map(|b| util::align_up(b.div_ceil(geo.ss), geo.grain)),
            staged: None,
        });
    }
    for e in &m.partitions {
        if !slots.iter().any(|s| s.number == e.number) {
            bail!(
                "manifest lists partition #{} but the saved partition table does not; \
                 the image is inconsistent",
                e.number
            );
        }
    }
    Ok(slots)
}

fn is_ext(e: &PartEntry) -> bool {
    matches!(e.fstype.as_deref(), Some("ext2") | Some("ext3") | Some("ext4"))
        && e.cloner.starts_with("partclone.ext")
}

fn fixed_list(slots: &[Slot]) -> String {
    slots
        .iter()
        .filter(|s| s.kind == Kind::Fixed)
        .map(|s| format!("#{} {}", s.number, s.fstype))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Reject `--part-size` values naming a partition that is missing or that
/// partclone cannot resize.
fn validate_pins(slots: &[Slot], opts: &ShrinkOpts) -> Result<()> {
    for number in opts.part_size.keys() {
        let s = slots
            .iter()
            .find(|s| s.number == *number)
            .with_context(|| format!("--part-size {}=...: no such partition in the image", number))?;
        if s.kind == Kind::Fixed {
            bail!(
                "--part-size {}=...: partition #{} is {}, which partclone cannot resize",
                number,
                number,
                s.fstype
            );
        }
    }
    Ok(())
}

/// Reject pins that ask for less than the filesystem can shrink to. Only
/// meaningful once `Slot::min` holds a real figure.
fn validate_pin_minimums(slots: &[Slot], geo: &Geometry) -> Result<()> {
    for s in slots {
        if let (Kind::Ext, Some(pin)) = (s.kind, s.pin) {
            if pin < s.min {
                bail!(
                    "--part-size {}=...: {} is below the {} this filesystem needs",
                    s.number,
                    util::human(pin * geo.ss),
                    util::human(s.min * geo.ss)
                );
            }
        }
    }
    Ok(())
}

/// A floor for planning before anything is staged: the used blocks from the
/// image header, padded, and never above the original size.
fn estimated_min(s: &Slot, geo: &Geometry) -> u64 {
    match s.kind {
        Kind::Ext => {
            let used = match &s.info {
                Some(i) => i.used_bytes,
                None => return util::align_up(s.orig, geo.grain),
            };
            // resize2fs needs room for relocated metadata; assume a quarter more.
            let est = used + used / 4 + (256 << 20);
            let sectors = util::align_up(
                std::cmp::max(est, EXT_FLOOR).div_ceil(geo.ss),
                geo.grain,
            );
            std::cmp::min(sectors, util::align_up(s.orig, geo.grain))
        }
        _ => s.min,
    }
}

/// Total scratch space the staging step will consume.
fn scratch_requirement(slots: &[Slot], m: &Manifest, image_dir: &Path) -> u64 {
    slots
        .iter()
        .filter(|s| s.kind == Kind::Ext)
        .map(|s| {
            let compressed = s
                .entry
                .and_then(|i| m.partitions[i].image_file.as_deref())
                .and_then(|f| std::fs::metadata(image_dir.join(f)).ok())
                .map(|md| md.len())
                .unwrap_or(0);
            s.stage_bytes(compressed)
        })
        .sum()
}

fn print_scratch(opts: &ShrinkOpts, need: u64) -> Result<()> {
    if need == 0 {
        return Ok(());
    }
    let free = util::fs_free_bytes(&opts.scratch).unwrap_or(0);
    println!(
        "Scratch  : {} — need {}, {} free   {}",
        opts.scratch.display(),
        util::human(need),
        util::human(free),
        if free >= need { "OK" } else { "NOT ENOUGH" }
    );
    Ok(())
}

fn require_scratch(opts: &ShrinkOpts, need: u64) -> Result<()> {
    if need == 0 {
        return Ok(());
    }
    std::fs::create_dir_all(&opts.scratch)
        .with_context(|| format!("creating scratch dir {}", opts.scratch.display()))?;
    let free = util::fs_free_bytes(&opts.scratch)?;
    if free < need {
        bail!(
            "not enough scratch space in {}: {} free, {} needed.\n\
             Point --scratch at a filesystem with more room.",
            opts.scratch.display(),
            util::human(free),
            util::human(need)
        );
    }
    Ok(())
}

/// Restore every ext image into its own sparse scratch file. All of them are
/// kept open until their data has been copied to the target.
fn stage_ext_partitions(
    image_dir: &Path,
    m: &Manifest,
    opts: &ShrinkOpts,
    slots: &mut [Slot],
    geo: &Geometry,
) -> Result<()> {
    let ext: Vec<usize> = (0..slots.len())
        .filter(|i| slots[*i].kind == Kind::Ext)
        .collect();
    for i in ext {
        let (number, orig, entry) = (slots[i].number, slots[i].orig, slots[i].entry);
        let p = &m.partitions[entry.expect("ext slot comes from the manifest")];
        println!(
            "\n[stage] partition #{} ({}) -> sparse file of {}",
            number,
            p.cloner,
            util::human(orig * geo.ss)
        );
        let staged = stage_one(image_dir, m, p, number, orig * geo.ss, opts)?;
        fsck(&staged.loopdev)?;
        slots[i].staged = Some(staged);
    }
    Ok(())
}

fn stage_one(
    image_dir: &Path,
    m: &Manifest,
    p: &PartEntry,
    number: u32,
    bytes: u64,
    opts: &ShrinkOpts,
) -> Result<Staged> {
    let file = opts
        .scratch
        .join(format!("dc-stage-{}-p{}.img", std::process::id(), number));
    let f = std::fs::File::create(&file)
        .with_context(|| format!("creating scratch file {}", file.display()))?;
    // btrfs/xfs: skip copy-on-write for a file written once and thrown away.
    // Must happen while the file is still empty, so before set_len.
    let _ = util::run(
        "chattr",
        &["+C", file.to_str().context("scratch path not UTF-8")?],
    );
    f.set_len(bytes)
        .with_context(|| format!("sizing scratch file {} to {} bytes", file.display(), bytes))?;
    drop(f);

    let loopdev = util::capture(
        "losetup",
        &[
            "--find",
            "--show",
            file.to_str().context("scratch path not UTF-8")?,
        ],
    )
    .context("attaching scratch file to a loop device")?
    .trim()
    .to_string();
    // The guard owns cleanup of both the file and the loop device from here.
    let staged = Staged {
        file,
        loopdev,
        keep: opts.keep_scratch,
    };
    restore::restore_partition(image_dir, p, &m.compression, &staged.loopdev)?;
    Ok(staged)
}

/// Hand out the target's space. Pinned partitions get exactly what was asked
/// for; unresizable ones keep their size; swap takes its scaled share; the ext
/// filesystems get their minimum plus a slice of the remainder, proportional to
/// how big they were originally.
fn allocate(
    slots: &mut [Slot],
    budget: u64,
    geo: &Geometry,
    opts: &ShrinkOpts,
    m: &Manifest,
) -> Result<()> {
    let source_sectors = std::cmp::max(1, m.disk_size_bytes / geo.ss);
    let swap_floor = util::align_up((512 << 20) / geo.ss, geo.grain);

    // floor = smallest acceptable, want = ideal size.
    let mut floor = vec![0u64; slots.len()];
    let mut want = vec![0u64; slots.len()];
    for (i, s) in slots.iter().enumerate() {
        match (s.kind, s.pin) {
            (Kind::Fixed, _) => {
                floor[i] = util::align_up(s.orig, geo.grain);
                want[i] = floor[i];
            }
            (_, Some(pin)) => {
                floor[i] = pin;
                want[i] = pin;
            }
            (Kind::Ext, None) => {
                floor[i] = s.min;
                want[i] = util::align_up(s.orig, geo.grain);
            }
            (Kind::Swap, None) => {
                // Carrying a 24 GiB swap onto a small disk would starve root, so
                // scale it by how much the disk shrank.
                let scaled = share_of(s.orig, geo.total_sectors, source_sectors);
                let w = match opts.swap_size {
                    Some(b) => b.div_ceil(geo.ss),
                    None => scaled.clamp(std::cmp::min(s.orig, swap_floor), s.orig),
                };
                want[i] = util::align_up(w, geo.grain);
                floor[i] = std::cmp::min(want[i], geo.grain);
            }
        }
    }

    let sum_floor: u64 = floor.iter().sum();
    if sum_floor > budget {
        bail!(
            "cannot fit: this image needs at least {} but {} of usable space is \
             available.\n{}",
            util::human(sum_floor * geo.ss),
            util::human(budget * geo.ss),
            floor_breakdown(slots, &floor, geo)
        );
    }
    let mut left = budget - sum_floor;

    // Swap gets topped up to what it asked for before ext takes the rest.
    for (i, s) in slots.iter().enumerate() {
        if s.kind == Kind::Swap {
            let top = std::cmp::min(want[i].saturating_sub(floor[i]), left);
            floor[i] += util::align_down(top, geo.grain);
            left -= util::align_down(top, geo.grain);
            if floor[i] < want[i] {
                println!(
                    "[plan] swap #{} squeezed to {} (wanted {})",
                    s.number,
                    util::human(floor[i] * geo.ss),
                    util::human(want[i] * geo.ss)
                );
            }
        }
    }

    // Remainder to the unpinned ext filesystems, proportional to original size.
    let share_base: u64 = slots
        .iter()
        .filter(|s| s.kind == Kind::Ext && s.pin.is_none())
        .map(|s| s.orig)
        .sum();
    for (i, s) in slots.iter().enumerate() {
        if s.kind == Kind::Ext && s.pin.is_none() {
            floor[i] += util::align_down(share_of(left, s.orig, share_base), geo.grain);
        }
    }

    for (i, s) in slots.iter_mut().enumerate() {
        s.alloc = floor[i];
    }
    Ok(())
}

fn floor_breakdown(slots: &[Slot], floor: &[u64], geo: &Geometry) -> String {
    slots
        .iter()
        .enumerate()
        .map(|(i, s)| {
            format!(
                "  #{} {} needs at least {}{}",
                s.number,
                s.fstype,
                util::human(floor[i] * geo.ss),
                match s.kind {
                    Kind::Fixed => " (not resizable)",
                    Kind::Ext if s.pin.is_some() => " (--part-size)",
                    _ => "",
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `pool * part / total`, in u128 so large sector counts cannot overflow.
fn share_of(pool: u64, part: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    ((pool as u128 * part as u128) / total as u128) as u64
}

/// Place the allocated partitions back-to-back from the first usable sector,
/// keeping their original order and numbers.
fn layout(pt: &Ptable, slots: &[Slot], geo: &Geometry) -> Result<Ptable> {
    let mut out = pt.clone();
    out.parts.retain(|p| slots.iter().any(|s| s.number == p.number));
    out.parts.sort_by_key(|p| {
        slots
            .iter()
            .position(|s| s.number == p.number)
            .unwrap_or(usize::MAX)
    });

    let mut cursor = geo.first;
    for p in out.parts.iter_mut() {
        let s = slots
            .iter()
            .find(|s| s.number == p.number)
            .expect("retained above");
        p.start = util::align_up(cursor, geo.grain);
        p.size = s.alloc;
        cursor = p.start + p.size;
    }
    if cursor.saturating_sub(1) > geo.last {
        bail!(
            "internal error: refitted layout ends at sector {} but the last usable \
             sector is {}",
            cursor - 1,
            geo.last
        );
    }
    Ok(out)
}

/// Which figures the plan table can show.
#[derive(Clone, Copy, PartialEq)]
enum PlanMode {
    /// Estimated from image headers, before staging.
    Projected,
    /// A header could not be read, so new sizes are not yet known.
    Unknown,
    /// Measured after staging; start/sector columns are real.
    Exact,
}

#[allow(clippy::too_many_arguments)]
fn print_plan(
    image_dir: &Path,
    m: &Manifest,
    target: &str,
    geo: &Geometry,
    slots: &[Slot],
    pt: Option<&Ptable>,
    budget: u64,
    mode: PlanMode,
) {
    println!("Image    : {}", image_dir.display());
    println!(
        "  created {}, source {} ({})",
        m.created_utc,
        m.source_device,
        util::human(m.disk_size_bytes)
    );
    println!(
        "Target   : {} ({}) — WILL BE ERASED",
        target,
        util::human(geo.target_bytes)
    );
    println!();
    if mode == PlanMode::Exact {
        println!("  #  fs        original         new        start      sectors  how");
    } else {
        println!("  #  fs        original         new   how");
    }
    let mut used = 0u64;
    for s in slots {
        let resizable = s.kind != Kind::Fixed;
        let how = match (s.kind, s.pin) {
            (Kind::Fixed, _) => format!("unchanged ({} cannot resize)", s.fstype),
            (_, Some(_)) => "pinned by --part-size".to_string(),
            (Kind::Ext, None) => match &s.info {
                Some(i) => format!("resized ({} in use)", util::human(i.used_bytes)),
                None => "resized (header unreadable, measured on staging)".to_string(),
            },
            (Kind::Swap, None) => "recreated by mkswap".to_string(),
        };
        // In Unknown mode only the unresizable partitions have a settled size.
        let new = if mode == PlanMode::Unknown && resizable {
            "?".to_string()
        } else {
            used += s.alloc;
            util::human(s.alloc * geo.ss)
        };
        match (mode, pt) {
            (PlanMode::Exact, Some(pt)) => {
                let p = pt.parts.iter().find(|p| p.number == s.number);
                println!(
                    "  {}  {:<8} {:>10}  {:>10}  {:>11}  {:>11}  {}",
                    s.number,
                    s.fstype,
                    util::human(s.orig * geo.ss),
                    new,
                    p.map(|p| p.start).unwrap_or(0),
                    p.map(|p| p.size).unwrap_or(0),
                    how
                );
            }
            _ => println!(
                "  {}  {:<8} {:>10}  {:>10}   {}",
                s.number,
                s.fstype,
                util::human(s.orig * geo.ss),
                new,
                how
            ),
        }
    }
    if mode == PlanMode::Unknown {
        println!(
            "{:>24}  {:>10}  usable on the target",
            "",
            util::human(budget * geo.ss)
        );
        return;
    }
    println!(
        "{:>24}  {:>10}  of {} usable",
        "",
        util::human(used * geo.ss),
        util::human(budget * geo.ss)
    );
    let spare = budget.saturating_sub(used);
    if spare > geo.grain {
        println!(
            "{:>24}  {:>10}  left unallocated",
            "",
            util::human(spare * geo.ss)
        );
    }
}

fn write_ptable(new_pt: &Ptable, target: &str, opts: &ShrinkOpts) -> Result<()> {
    let script = new_pt.render(target);
    let path = opts
        .scratch
        .join(format!("dc-ptable-{}.sfdisk", std::process::id()));
    std::fs::write(&path, &script).with_context(|| format!("writing {}", path.display()))?;
    println!("\nWriting refitted partition table to {}", target);
    let res = util::run_pipeline(&format!(
        "sfdisk --force '{}' < '{}'",
        restore::esc(target),
        restore::esc(path.to_str().context("scratch path not UTF-8")?)
    ));
    let _ = std::fs::remove_file(&path);
    res?;
    util::run("partprobe", &[target]).ok();
    let _ = util::run("udevadm", &["settle"]);
    Ok(())
}

fn resize_staged(staged: &Staged, bytes: u64, sectors: u64) -> Result<()> {
    let cur = std::fs::metadata(&staged.file)?.len();
    let sect_arg = format!("{}s", sectors);
    if bytes >= cur {
        // Grow the backing store first, then the filesystem into it.
        set_len(&staged.file, bytes)?;
        util::run("losetup", &["-c", &staged.loopdev])?;
        fsck(&staged.loopdev)?;
        util::run("resize2fs", &[&staged.loopdev, &sect_arg])?;
    } else {
        // Shrink the filesystem first, or truncation would eat live data.
        fsck(&staged.loopdev)?;
        util::run("resize2fs", &[&staged.loopdev, &sect_arg])?;
        set_len(&staged.file, bytes)?;
        util::run("losetup", &["-c", &staged.loopdev])?;
    }
    fsck(&staged.loopdev)
}

fn set_len(file: &Path, bytes: u64) -> Result<()> {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(file)
        .with_context(|| format!("opening {}", file.display()))?;
    f.set_len(bytes)
        .with_context(|| format!("resizing {} to {} bytes", file.display(), bytes))
}

/// `resize2fs -P` reports the minimum in filesystem blocks.
fn fs_min_bytes(dev: &str) -> Result<u64> {
    let out = util::capture("resize2fs", &["-P", dev])?;
    let blocks: u64 = out
        .rsplit(':')
        .next()
        .and_then(|s| s.trim().parse().ok())
        .with_context(|| format!("could not read a minimum size from: {}", out.trim()))?;
    Ok(blocks * fs_block_size(dev)?)
}

fn fs_block_size(dev: &str) -> Result<u64> {
    let out = util::capture("tune2fs", &["-l", dev])?;
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("Block size:") {
            return v.trim().parse().context("parsing Block size");
        }
    }
    bail!("tune2fs did not report a block size for {}", dev)
}

/// e2fsck exit codes 1 and 2 mean "errors corrected", which is success here.
fn fsck(dev: &str) -> Result<()> {
    let status = std::process::Command::new("e2fsck")
        .args(["-f", "-y", dev])
        .status()
        .context("spawning e2fsck")?;
    match status.code() {
        Some(0..=3) => Ok(()),
        Some(c) => bail!("e2fsck on {} failed (exit {})", dev, c),
        None => bail!("e2fsck on {} terminated by signal", dev),
    }
}
