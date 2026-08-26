use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

/// True if running as root (uid 0).
pub fn is_root() -> bool {
    // SAFETY: getuid is always safe; no args, no global state mutated.
    unsafe { libc_getuid() == 0 }
}

extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// Abort unless running as root. Disk imaging needs raw block access.
pub fn require_root() -> Result<()> {
    if !is_root() {
        bail!("must run as root (raw block-device access). Re-run with sudo.");
    }
    Ok(())
}

/// True if the named program is on PATH.
pub fn have(prog: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {} >/dev/null 2>&1", prog))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a command, capture stdout as a String, error on non-zero exit.
pub fn capture(prog: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .with_context(|| format!("spawning {}", prog))?;
    if !out.status.success() {
        bail!(
            "{} {:?} failed ({}): {}",
            prog,
            args,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a command inheriting stdio (progress visible), error on non-zero exit.
pub fn run(prog: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(prog)
        .args(args)
        .status()
        .with_context(|| format!("spawning {}", prog))?;
    if !status.success() {
        bail!("{} {:?} failed: {}", prog, args, status);
    }
    Ok(())
}

/// Run a pipeline via `sh -euo pipefail -c`. stderr/stdout inherited so the
/// user sees partclone's progress bar.
pub fn run_pipeline(script: &str) -> Result<()> {
    let status = Command::new("sh")
        .arg("-euo")
        .arg("pipefail")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .status()
        .context("spawning shell pipeline")?;
    if !status.success() {
        bail!("pipeline failed ({}): {}", status, script);
    }
    Ok(())
}

/// Refuse if the device is currently mounted anywhere.
pub fn assert_not_mounted(dev: &Path) -> Result<()> {
    let mounts = std::fs::read_to_string("/proc/mounts").context("reading /proc/mounts")?;
    let dev_str = dev
        .to_str()
        .ok_or_else(|| anyhow!("device path not UTF-8"))?;
    for line in mounts.lines() {
        if let Some(src) = line.split_whitespace().next() {
            if src == dev_str || src.starts_with(&format!("{}p", dev_str)) || starts_part(src, dev_str)
            {
                bail!(
                    "{} (or a partition of it) is mounted: {}\nUnmount it first.",
                    dev_str,
                    line
                );
            }
        }
    }
    Ok(())
}

/// e.g. /dev/sda matches /dev/sda1; /dev/nvme0n1 matches /dev/nvme0n1p1.
fn starts_part(src: &str, dev: &str) -> bool {
    if let Some(rest) = src.strip_prefix(dev) {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
            || rest.starts_with('p') && rest[1..].chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
}

/// Parse a human size: bare bytes, or K/M/G/T (powers of 1024, optional "iB"/"B").
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    if t.is_empty() {
        bail!("empty size");
    }
    let lower = t.to_ascii_lowercase();
    let digits_end = lower
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(lower.len());
    let (num, unit) = lower.split_at(digits_end);
    let n: u64 = num.parse().with_context(|| format!("bad size '{}'", s))?;
    let unit = unit.trim().trim_end_matches("ib").trim_end_matches('b');
    let mult: u64 = match unit {
        "" => 1,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        other => bail!("unknown size unit '{}' in '{}'", other, s),
    };
    n.checked_mul(mult)
        .ok_or_else(|| anyhow!("size '{}' overflows", s))
}

/// Size of a block device in bytes.
pub fn device_size_bytes(dev: &str) -> Result<u64> {
    let out = capture("blockdev", &["--getsize64", dev])?;
    out.trim()
        .parse()
        .with_context(|| format!("parsing blockdev --getsize64 {}", dev))
}

/// Logical sector size of a block device in bytes.
pub fn logical_sector_size(dev: &str) -> Result<u64> {
    let out = capture("blockdev", &["--getss", dev])?;
    out.trim()
        .parse()
        .with_context(|| format!("parsing blockdev --getss {}", dev))
}

/// Free bytes on the filesystem holding `path`.
pub fn fs_free_bytes(path: &Path) -> Result<u64> {
    let p = path.to_str().ok_or_else(|| anyhow!("path not UTF-8"))?;
    let out = capture("df", &["-B1", "--output=avail", p])?;
    out.lines()
        .nth(1)
        .and_then(|l| l.trim().parse().ok())
        .ok_or_else(|| anyhow!("could not read free space for {}", p))
}

/// Round `v` up to the next multiple of `grain`.
pub fn align_up(v: u64, grain: u64) -> u64 {
    if grain == 0 {
        return v;
    }
    v.div_ceil(grain) * grain
}

/// Round `v` down to a multiple of `grain`.
pub fn align_down(v: u64, grain: u64) -> u64 {
    if grain == 0 {
        return v;
    }
    (v / grain) * grain
}

/// Human-readable byte count for plan output.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}

/// Run a shell pipeline and capture raw stdout. Unlike `run_pipeline` this uses
/// a plain `sh -c` (no `pipefail`), so a producer killed by an early-closing
/// consumer — `... | head -c N` — is not treated as failure.
pub fn capture_pipeline(script: &str) -> Result<Vec<u8>> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .output()
        .context("spawning shell pipeline")?;
    if !out.status.success() {
        bail!("pipeline failed ({}): {}", out.status, script);
    }
    Ok(out.stdout)
}
