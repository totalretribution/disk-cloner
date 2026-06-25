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
