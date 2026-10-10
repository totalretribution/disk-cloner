use crate::checksum::{self, Covers};
use crate::lsblk;
use crate::restore::confirm_target;
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

#[derive(Args)]
pub struct WriteArgs {
    /// Raw disk image to write: .iso or .img, optionally zstd-compressed (.zst).
    pub image: PathBuf,

    /// Target disk to overwrite, e.g. /dev/sdc (ALL DATA WILL BE DESTROYED).
    pub device: PathBuf,

    /// Required to actually write. Without it, prints the plan and stops.
    #[arg(long)]
    pub yes: bool,

    /// Proceed even if the target appears mounted or in use (DANGEROUS).
    #[arg(long)]
    pub force: bool,

    /// Expected sha256: a checksum file (sha256sum or BSD format, may list
    /// several files) or a bare hash of the image file. Default: use
    /// <IMAGE>.sha256 if it exists.
    #[arg(long, value_name = "FILE|HEX")]
    pub sha256: Option<String>,

    /// Do not look for or verify a checksum.
    #[arg(long, conflicts_with = "sha256")]
    pub no_checksum: bool,
}

/// Bytes handed to each write(2), matching `dd bs=4M`.
const CHUNK: usize = 4 << 20;

/// fdatasync this often so dirty pages never pile up: progress tracks what has
/// actually reached the disk, and the final flush is short.
const SYNC_EVERY: u64 = 64 << 20;

/// O_EXCL on a block device: the open fails with EBUSY while the disk or any of
/// its partitions is mounted or claimed. Value is the generic Linux one (x86,
/// arm, aarch64, riscv).
const O_EXCL: i32 = 0o200;

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Largest window the decoder accepts, as log2 bytes. libzstd defaults to 27
/// (128 MiB) and rejects images made with `--long=28` and up; 31 is the zstd
/// format maximum on 64-bit.
const WINDOW_LOG_MAX: u32 = 31;

/// Skippable frames use magics 0x184D2A50..=0x184D2A5F. pzstd starts its output
/// with one, so a zstd image need not begin with the normal frame magic.
fn is_skippable(head: &[u8]) -> bool {
    head.len() >= 4 && head[0] & 0xF0 == 0x50 && head[1..4] == [0x2A, 0x4D, 0x18]
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    Raw,
    Zstd,
}

/// Decide the image format from its first bytes, not its extension. Refuse
/// compressors we cannot decode rather than writing compressed bytes raw.
fn sniff(head: &[u8]) -> Result<Format> {
    if head.starts_with(&ZSTD_MAGIC) || is_skippable(head) {
        return Ok(Format::Zstd);
    }
    if head.starts_with(&[0x1F, 0x8B, 0x08]) {
        bail!("image is gzip-compressed; only raw and zstd images are supported");
    }
    if head.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
        bail!("image is xz-compressed; only raw and zstd images are supported");
    }
    Ok(Format::Raw)
}

/// Read until `buf` is full or EOF, so every write but the last is a whole chunk.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Counts bytes pulled from the image file, so progress for a compressed image
/// can be shown against its on-disk size.
struct Counting<R> {
    inner: R,
    count: Rc<Cell<u64>>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.set(self.count.get() + n as u64);
        Ok(n)
    }
}

/// What `probe_image` learns from an image file without decompressing it.
pub struct Probe {
    pub format: Format,
    pub file_size: u64,
    /// Exact uncompressed size, or None for a zstd image whose frames do not all
    /// record their size (e.g. compressed from a pipe). `measure` fills the gap.
    pub raw_size: Option<u64>,
}

pub fn probe_image(path: &Path) -> Result<Probe> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let file_size = meta.len();
    if file_size == 0 {
        bail!("{} is empty", path.display());
    }
    // 18 bytes covers the largest zstd frame header.
    let mut head = [0u8; 18];
    let n = read_full(&mut f, &mut head)?;
    let format = sniff(&head[..n])?;
    let raw_size = match format {
        Format::Raw => Some(file_size),
        // If the headers cannot be walked, leave it to the decoder: `measure`
        // either counts the bytes or reports what is wrong with the file.
        Format::Zstd => scan_frames(&f, file_size).ok().flatten(),
    };
    Ok(Probe {
        format,
        file_size,
        raw_size,
    })
}

/// Length of a zstd frame header (magic included), from its descriptor byte.
fn frame_header_len(fhd: u8) -> u64 {
    let single_segment = fhd & 0x20 != 0;
    let window = if single_segment { 0 } else { 1 };
    let dict_id = [0, 1, 2, 4][(fhd & 0x03) as usize];
    let content_size = match fhd >> 6 {
        0 if single_segment => 1,
        0 => 0,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    4 + 1 + window + dict_id + content_size
}

/// Total uncompressed size of a zstd file, found by hopping from header to
/// header (frame headers, then each block's 3-byte header) without
/// decompressing. Ok(None) if any frame does not record its content size.
fn scan_frames(f: &File, file_size: u64) -> Result<Option<u64>> {
    let at = |buf: &mut [u8], off: u64| {
        f.read_exact_at(buf, off)
            .with_context(|| format!("zstd image truncated at offset {}", off))
    };
    let mut off = 0u64;
    let mut total = Some(0u64);
    while off < file_size {
        let mut head = [0u8; 8];
        at(&mut head[..4], off)?;
        if is_skippable(&head) {
            at(&mut head[4..], off + 4)?;
            off += 8 + u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as u64;
            continue;
        }
        if head[..4] != ZSTD_MAGIC {
            bail!("no zstd frame at offset {}", off);
        }
        at(&mut head[4..5], off + 4)?;
        let fhd = head[4];
        let mut header = vec![0u8; frame_header_len(fhd) as usize];
        at(&mut header, off)?;
        let size = zstd::zstd_safe::get_frame_content_size(&header)
            .map_err(|_| anyhow!("bad zstd frame header at offset {}", off))?;
        total = total.zip(size).map(|(t, s)| t + s);
        off += header.len() as u64;

        loop {
            let mut bh = [0u8; 3];
            at(&mut bh, off)?;
            let v = u32::from_le_bytes([bh[0], bh[1], bh[2], 0]);
            let size = (v >> 3) as u64;
            off += 3 + match (v >> 1) & 0x03 {
                0 | 2 => size, // raw / compressed: that many bytes follow
                1 => 1,        // RLE: one byte, repeated `size` times
                _ => bail!("reserved zstd block type at offset {}", off),
            };
            if v & 1 == 1 {
                break;
            }
        }
        if fhd & 0x04 != 0 {
            off += 4; // content checksum
        }
    }
    if off > file_size {
        bail!("zstd image is truncated");
    }
    Ok(total)
}

/// Feed the image's data (decompressed if needed) to `sink` in CHUNK-sized
/// pieces, never past `limit` bytes. Returns the byte count and, if `hash` is
/// set, the sha256 of the data.
fn stream(
    image: &Path,
    format: Format,
    limit: u64,
    hash: bool,
    progress: &mut Progress,
    mut sink: impl FnMut(&[u8], u64) -> Result<()>,
) -> Result<(u64, Option<String>)> {
    let read = Rc::new(Cell::new(0u64));
    let file = Counting {
        inner: File::open(image).with_context(|| format!("opening {}", image.display()))?,
        count: Rc::clone(&read),
    };
    let mut src: Box<dyn Read> = match format {
        Format::Raw => Box::new(file),
        Format::Zstd => {
            let mut d = zstd::Decoder::new(file).context("starting zstd decoder")?;
            d.window_log_max(WINDOW_LOG_MAX)
                .context("configuring zstd decoder")?;
            Box::new(d)
        }
    };
    let mut hasher = hash.then(Sha256::new);

    let mut buf = vec![0u8; CHUNK];
    let mut written = 0u64;
    loop {
        let n = read_full(&mut src, &mut buf).context("reading image")?;
        if n == 0 {
            break;
        }
        if written + n as u64 > limit {
            bail!(
                "image is larger than the target ({}); stopped after writing {}",
                util::human(limit),
                util::human(written)
            );
        }
        sink(&buf[..n], written)?;
        if let Some(h) = hasher.as_mut() {
            h.update(&buf[..n]);
        }
        written += n as u64;
        progress.update(written, read.get());
    }
    progress.finish(written, read.get());
    Ok((written, hasher.map(|h| checksum::to_hex(&h.finalize()))))
}

/// Write the image onto `dst`, never past `limit` bytes. Returns the bytes
/// written and, if `hash` is set, their sha256. `dst` is synced at the end.
fn copy_image(
    image: &Path,
    format: Format,
    dst: &mut File,
    limit: u64,
    hash: bool,
    progress: &mut Progress,
) -> Result<(u64, Option<String>)> {
    let mut since_sync = 0u64;
    let out = stream(image, format, limit, hash, progress, |chunk, offset| {
        dst.write_all(chunk)
            .with_context(|| format!("writing at offset {}", offset))?;
        since_sync += chunk.len() as u64;
        if since_sync >= SYNC_EVERY {
            dst.sync_data().context("flushing to target")?;
            since_sync = 0;
        }
        Ok(())
    })?;
    eprintln!("Flushing to disk...");
    dst.sync_all().context("flushing to target")?;
    Ok(out)
}

/// Decompress the image and throw the output away, to learn its exact size
/// and, if `hash` is set, its sha256 before anything is written.
fn measure(image: &Path, hash: bool, progress: &mut Progress) -> Result<(u64, Option<String>)> {
    stream(image, Format::Zstd, u64::MAX, hash, progress, |_, _| Ok(()))
}

/// Single self-overwriting status line on stderr, like `dd status=progress`.
struct Progress {
    start: Instant,
    last: Option<Instant>,
    /// Size of the image file; the percentage tracks how much of it was read.
    file_size: u64,
    /// What is happening to the bytes, e.g. "written".
    verb: &'static str,
    quiet: bool,
}

impl Progress {
    fn new(file_size: u64, verb: &'static str) -> Self {
        Progress {
            start: Instant::now(),
            last: None,
            file_size,
            verb,
            quiet: false,
        }
    }

    fn update(&mut self, written: u64, read: u64) {
        let now = Instant::now();
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < Duration::from_millis(500))
        {
            return;
        }
        self.last = Some(now);
        self.print(written, read);
    }

    fn finish(&mut self, written: u64, read: u64) {
        self.print(written, read);
        if !self.quiet {
            eprintln!();
        }
    }

    fn print(&self, written: u64, read: u64) {
        if self.quiet {
            return;
        }
        let secs = self.start.elapsed().as_secs_f64().max(0.001);
        let pct = (read as f64 / self.file_size as f64 * 100.0).min(100.0);
        eprint!(
            "\r  {} {}, {}/s, {:.0}%   ",
            util::human(written),
            self.verb,
            util::human((written as f64 / secs) as u64),
            pct
        );
        io::stderr().flush().ok();
    }
}

pub fn run(args: WriteArgs) -> Result<()> {
    let img = probe_image(&args.image)?;

    let dev = lsblk::probe(&args.device)?;
    if dev.dtype != "disk" && dev.dtype != "loop" {
        bail!(
            "{} is type '{}', not a whole disk. Pass the disk (e.g. /dev/sdc), not a partition.",
            dev.dev_path(),
            dev.dtype
        );
    }
    let target = dev.dev_path();
    let dev_size = dev
        .size
        .ok_or_else(|| anyhow!("lsblk did not report a size for {}", target))?;

    if !args.force {
        util::assert_not_mounted(&args.device)?;
    }

    let expected = if args.no_checksum {
        None
    } else {
        checksum::resolve(
            args.sha256.as_deref(),
            &args.image,
            img.format == Format::Zstd,
        )?
    };
    // A hash of the file itself is checked now, so a bad download is refused
    // before the target is touched.
    if let Some(e) = expected.as_ref().filter(|e| e.covers == Covers::File) {
        eprintln!("Checking sha256 of {}...", args.image.display());
        let actual = checksum::hash_file(&args.image)?;
        if actual != e.hex {
            bail!(
                "sha256 mismatch for {}\n  expected {} (from {})\n  actual   {}\n\
                 The image is corrupt or incomplete. Download it again.",
                args.image.display(),
                e.hex,
                e.source,
                actual
            );
        }
    }

    // A size missing from the headers, or a hash of the decompressed data,
    // needs one decompression pass up front. Decompressing is far faster than
    // writing a disk, and it means neither surprise arrives mid-write.
    let inner = expected
        .as_ref()
        .filter(|e| e.covers == Covers::Decompressed);
    let raw_size = match img.raw_size {
        Some(n) if inner.is_none() => n,
        _ => {
            if img.raw_size.is_none() {
                eprintln!("Uncompressed size not recorded in the image; measuring it...");
            } else {
                eprintln!("Checking sha256 of the decompressed image...");
            }
            let mut progress = Progress::new(img.file_size, "decompressed");
            let (n, actual) = measure(&args.image, inner.is_some(), &mut progress)?;
            if let (Some(e), Some(actual)) = (inner, actual) {
                if actual != e.hex {
                    bail!(
                        "sha256 mismatch for the decompressed image\n  expected {} (from {})\n  \
                         actual   {}\nThe image is corrupt or incomplete. Download it again.",
                        e.hex,
                        e.source,
                        actual
                    );
                }
            }
            n
        }
    };
    if raw_size > dev_size {
        bail!(
            "image needs {} but target {} is only {}",
            util::human(raw_size),
            target,
            util::human(dev_size)
        );
    }

    println!("Write image : {}", args.image.display());
    match img.format {
        Format::Raw => println!("  format    : raw ({})", util::human(raw_size)),
        Format::Zstd => println!(
            "  format    : zstd ({} compressed, {} uncompressed)",
            util::human(img.file_size),
            util::human(raw_size)
        ),
    }
    match &expected {
        Some(e) if e.covers == Covers::File => {
            println!("  checksum  : sha256 OK (from {})", e.source)
        }
        Some(e) => println!(
            "  checksum  : sha256 of decompressed data OK (from {})",
            e.source
        ),
        None if args.no_checksum => println!("  checksum  : skipped (--no-checksum)"),
        None => println!(
            "  checksum  : none (no {}.sha256; pass --sha256 to check one)",
            args.image.display()
        ),
    }
    println!(
        "Target disk (WILL BE ERASED): {} ({})",
        target,
        util::human(dev_size)
    );

    if !args.yes {
        println!("\nRe-run with --yes to perform the destructive write.");
        return Ok(());
    }
    util::require_root()?;
    confirm_target(&target)?;

    let mut dst = OpenOptions::new()
        .write(true)
        .custom_flags(if args.force { 0 } else { O_EXCL })
        .open(&target)
        .map_err(|e| {
            if e.raw_os_error() == Some(16) {
                anyhow!(
                    "{} is busy (mounted, or held by another process). Unmount it first.",
                    target
                )
            } else {
                anyhow!(e).context(format!("opening {} for writing", target))
            }
        })?;

    println!("\nWriting {} -> {}", args.image.display(), target);
    let start = Instant::now();
    let mut progress = Progress::new(img.file_size, "written");
    let inner = expected.filter(|e| e.covers == Covers::Decompressed);
    let (written, actual) = copy_image(
        &args.image,
        img.format,
        &mut dst,
        dev_size,
        inner.is_some(),
        &mut progress,
    )?;
    drop(dst);

    if let (Some(e), Some(actual)) = (&inner, &actual) {
        if *actual != e.hex {
            bail!(
                "sha256 mismatch: the data written to {} does not match {}\n  \
                 expected {}\n  actual   {}\nThe target holds a bad image. Do not boot it.",
                target,
                e.source,
                e.hex,
                actual
            );
        }
        println!("sha256 of written data matches {}", e.source);
    }

    // Have the kernel pick up the partition table the image just laid down.
    if util::have("partprobe") {
        util::run("partprobe", &[&target]).ok();
    }

    println!(
        "Wrote {} to {} in {:.0}s",
        util::human(written),
        target,
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("disk-cloner-write-{}-{}", std::process::id(), name))
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn quiet() -> Progress {
        Progress {
            quiet: true,
            ..Progress::new(1, "written")
        }
    }

    fn run_copy(src: &Path, limit: u64) -> Result<Vec<u8>> {
        let img = probe_image(src)?;
        let out = tmp(&format!(
            "{}.out",
            src.file_name().unwrap().to_string_lossy()
        ));
        let mut f = File::create(&out)?;
        let res = copy_image(src, img.format, &mut f, limit, true, &mut quiet());
        let data = std::fs::read(&out)?;
        std::fs::remove_file(&out).ok();
        res.map(|(n, hash)| {
            assert_eq!(hash.unwrap(), checksum::to_hex(&Sha256::digest(&data)));
            assert_eq!(n as usize, data.len());
            data
        })
    }

    #[test]
    fn sniffs_formats() {
        assert_eq!(
            sniff(&[0x28, 0xB5, 0x2F, 0xFD, 0x04]).unwrap(),
            Format::Zstd
        );
        assert_eq!(sniff(&[0xEB, 0x63, 0x90]).unwrap(), Format::Raw);
        assert_eq!(sniff(&[]).unwrap(), Format::Raw);
        assert!(sniff(&[0x1F, 0x8B, 0x08, 0x00]).is_err());
        assert!(sniff(b"\xFD7zXZ\x00\x00").is_err());
    }

    #[test]
    fn copies_raw_image() {
        // Not a multiple of CHUNK, so the short final chunk is exercised.
        let data = pattern(CHUNK * 2 + 12345);
        let src = tmp("raw.img");
        std::fs::write(&src, &data).unwrap();
        let out = run_copy(&src, u64::MAX).unwrap();
        std::fs::remove_file(&src).ok();
        assert_eq!(out, data);
    }

    #[test]
    fn decompresses_zstd_image() {
        let data = pattern(CHUNK * 3 + 777);
        let src = tmp("img.iso.zst");
        // bulk::compress records the content size in the frame header, as the
        // zstd CLI does for files.
        std::fs::write(&src, zstd::bulk::compress(&data, 3).unwrap()).unwrap();
        let img = probe_image(&src).unwrap();
        assert_eq!(img.format, Format::Zstd);
        assert_eq!(img.raw_size, Some(data.len() as u64));
        let out = run_copy(&src, u64::MAX).unwrap();
        std::fs::remove_file(&src).ok();
        assert_eq!(out, data);
    }

    #[test]
    fn decompresses_multi_frame_zstd() {
        let a = pattern(5000);
        let b = pattern(CHUNK + 3);
        let mut z = zstd::encode_all(&a[..], 1).unwrap();
        z.extend(zstd::encode_all(&b[..], 1).unwrap());
        let src = tmp("multi.zst");
        std::fs::write(&src, z).unwrap();
        // encode_all streams, so neither frame records its size.
        assert_eq!(probe_image(&src).unwrap().raw_size, None);
        let out = run_copy(&src, u64::MAX).unwrap();
        std::fs::remove_file(&src).ok();
        assert_eq!(out, [a, b].concat());
    }

    #[test]
    fn handles_leading_skippable_frame() {
        // pzstd layout: a skippable frame, then the real frame.
        let data = pattern(CHUNK + 99);
        let mut z = vec![0x50, 0x2A, 0x4D, 0x18, 4, 0, 0, 0, 1, 2, 3, 4];
        z.extend(zstd::bulk::compress(&data, 3).unwrap());
        let src = tmp("pzstd.zst");
        std::fs::write(&src, z).unwrap();
        let img = probe_image(&src).unwrap();
        assert_eq!(img.format, Format::Zstd);
        assert_eq!(img.raw_size, Some(data.len() as u64));
        let out = run_copy(&src, u64::MAX).unwrap();
        std::fs::remove_file(&src).ok();
        assert_eq!(out, data);
    }

    #[test]
    fn decodes_long_window() {
        // A 256 MiB window (--long=28) is past libzstd's default decode limit.
        let data = pattern(1 << 20);
        let mut enc = zstd::Encoder::new(Vec::new(), 3).unwrap();
        enc.long_distance_matching(true).unwrap();
        enc.window_log(28).unwrap();
        enc.write_all(&data).unwrap();
        let src = tmp("long.zst");
        std::fs::write(&src, enc.finish().unwrap()).unwrap();
        let out = run_copy(&src, u64::MAX).unwrap();
        std::fs::remove_file(&src).ok();
        assert_eq!(out, data);
    }

    /// Incompressible bytes, so zstd emits raw blocks.
    fn noise(len: usize) -> Vec<u8> {
        let mut x = 0x9E3779B97F4A7C15u64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn scan(bytes: &[u8], name: &str) -> Result<Option<u64>> {
        let src = tmp(name);
        std::fs::write(&src, bytes).unwrap();
        let res = scan_frames(&File::open(&src).unwrap(), bytes.len() as u64);
        std::fs::remove_file(&src).ok();
        res
    }

    #[test]
    fn scans_frame_sizes() {
        // Compressed, raw (noise) and RLE (zeros) blocks, with a checksum.
        let data = [pattern(300_000), noise(400_000), vec![0u8; 500_000]].concat();
        let mut c = zstd::bulk::Compressor::new(3).unwrap();
        c.include_checksum(true).unwrap();
        let one = c.compress(&data).unwrap();
        assert_eq!(scan(&one, "scan1").unwrap(), Some(data.len() as u64));

        // Several frames plus pzstd-style skippable frames: sizes add up.
        let small = zstd::bulk::compress(&pattern(7), 1).unwrap();
        let skip = [0x5E, 0x2A, 0x4D, 0x18, 2, 0, 0, 0, 9, 9];
        let multi = [&skip[..], &one, &skip, &small].concat();
        assert_eq!(scan(&multi, "scan2").unwrap(), Some(data.len() as u64 + 7));

        // Streaming compression does not record the size.
        let streamed = zstd::encode_all(&data[..], 3).unwrap();
        assert_eq!(scan(&streamed, "scan3").unwrap(), None);
        let mixed = [&one[..], &streamed].concat();
        assert_eq!(scan(&mixed, "scan4").unwrap(), None);
    }

    #[test]
    fn scan_rejects_damaged_files() {
        let one = zstd::bulk::compress(&noise(300_000), 3).unwrap();
        assert!(scan(&one[..one.len() - 1], "trunc1").is_err());
        assert!(scan(&one[..one.len() / 2], "trunc2").is_err());
        assert!(scan(&[&one[..], b"junk"].concat(), "junk").is_err());
    }

    #[test]
    fn measures_unrecorded_size() {
        let data = pattern(CHUNK * 2 + 5);
        let src = tmp("measure.zst");
        std::fs::write(&src, zstd::encode_all(&data[..], 3).unwrap()).unwrap();
        assert_eq!(probe_image(&src).unwrap().raw_size, None);
        let (n, hash) = measure(&src, true, &mut quiet()).unwrap();
        std::fs::remove_file(&src).ok();
        assert_eq!(n, data.len() as u64);
        assert_eq!(hash.unwrap(), checksum::to_hex(&Sha256::digest(&data)));
    }

    #[test]
    fn stops_at_target_size() {
        let src = tmp("big.img");
        std::fs::write(&src, pattern(CHUNK + 10)).unwrap();
        let err = run_copy(&src, CHUNK as u64).unwrap_err();
        std::fs::remove_file(&src).ok();
        assert!(
            err.to_string().contains("larger than the target"),
            "{}",
            err
        );
    }

    #[test]
    fn rejects_truncated_zstd() {
        let z = zstd::encode_all(&pattern(CHUNK)[..], 3).unwrap();
        let src = tmp("trunc.zst");
        std::fs::write(&src, &z[..z.len() / 2]).unwrap();
        let res = run_copy(&src, u64::MAX);
        std::fs::remove_file(&src).ok();
        assert!(res.is_err());
    }
}
