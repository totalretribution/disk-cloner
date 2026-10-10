use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// What an expected hash was computed over.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Covers {
    /// The image file as it sits on disk (e.g. the .iso.zst itself).
    File,
    /// The decompressed stream, i.e. the bytes that end up on the target.
    Decompressed,
}

#[derive(Debug, Clone)]
pub struct Expected {
    /// Lowercase hex.
    pub hex: String,
    pub covers: Covers,
    /// Where the hash came from, for messages.
    pub source: String,
}

/// One line of a checksum file: the hash and, if given, the file it names.
#[derive(Debug, PartialEq)]
struct Entry {
    hex: String,
    name: Option<String>,
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse `sha256sum` output (`hex  name`, `hex *name`), BSD style
/// (`SHA256 (name) = hex`) or a bare hash. Blank lines and `#` comments are
/// skipped.
fn parse(text: &str) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let entry = if let Some(rest) = line.strip_prefix("SHA256 (") {
            rest.rsplit_once(") = ").map(|(name, hex)| Entry {
                hex: hex.trim().to_ascii_lowercase(),
                name: Some(name.to_string()),
            })
        } else {
            let (hex, name) = match line.split_once(char::is_whitespace) {
                Some((hex, rest)) => {
                    let name = rest.trim_start().trim_start_matches('*');
                    (hex, (!name.is_empty()).then(|| name.to_string()))
                }
                None => (line, None),
            };
            Some(Entry {
                hex: hex.to_ascii_lowercase(),
                name,
            })
        };
        match entry {
            Some(e) if is_sha256_hex(&e.hex) => out.push(e),
            _ => bail!("line {}: not a sha256 checksum: {}", i + 1, raw),
        }
    }
    if out.is_empty() {
        bail!("no checksums found");
    }
    Ok(out)
}

fn basename(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// Pick the entry for `image`. A hash named after the image covers the file; one
/// named after the image minus `.zst` covers the decompressed data.
fn select(entries: Vec<Entry>, image: &Path, compressed: bool, source: &str) -> Result<Expected> {
    let file_name = image
        .file_name()
        .and_then(|n| n.to_str())
        .context("image file name not UTF-8")?;
    let inner_name = if compressed {
        file_name.strip_suffix(".zst")
    } else {
        None
    };

    // A lone unnamed hash is taken to be of the file given.
    if let [Entry { name: None, .. }] = entries.as_slice() {
        return Ok(Expected {
            hex: entries.into_iter().next().unwrap().hex,
            covers: Covers::File,
            source: source.to_string(),
        });
    }

    let mut inner = None;
    for e in &entries {
        let Some(name) = e.name.as_deref().map(basename) else {
            continue;
        };
        if name == file_name {
            return Ok(Expected {
                hex: e.hex.clone(),
                covers: Covers::File,
                source: source.to_string(),
            });
        }
        if Some(name) == inner_name {
            inner = Some(e.hex.clone());
        }
    }
    if let Some(hex) = inner {
        return Ok(Expected {
            hex,
            covers: Covers::Decompressed,
            source: source.to_string(),
        });
    }

    let listed: Vec<&str> = entries.iter().filter_map(|e| e.name.as_deref()).collect();
    bail!(
        "{} has no checksum for {} (it lists: {})",
        source,
        file_name,
        listed.join(", ")
    )
}

/// Work out the expected hash for `image`:
/// - `--sha256 HEX` is a hash of the image file as given;
/// - `--sha256 FILE` is a checksum file to read;
/// - otherwise `<image>.sha256` is used if it exists.
pub fn resolve(arg: Option<&str>, image: &Path, compressed: bool) -> Result<Option<Expected>> {
    let path = match arg {
        Some(s) if is_sha256_hex(s.trim()) => {
            return Ok(Some(Expected {
                hex: s.trim().to_ascii_lowercase(),
                covers: Covers::File,
                source: "--sha256".to_string(),
            }))
        }
        Some(s) => Path::new(s).to_path_buf(),
        None => {
            let mut p = image.as_os_str().to_owned();
            p.push(".sha256");
            let p = Path::new(&p).to_path_buf();
            if !p.exists() {
                return Ok(None);
            }
            p
        }
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading checksum file {}", path.display()))?;
    let source = path.display().to_string();
    let entries = parse(&text).with_context(|| format!("parsing {}", source))?;
    select(entries, image, compressed, &source).map(Some)
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// sha256 of a whole file, as lowercase hex.
pub fn hash_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(to_hex(&h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4";
    const B: &str = "b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c";

    fn entry(hex: &str, name: Option<&str>) -> Entry {
        Entry {
            hex: hex.into(),
            name: name.map(Into::into),
        }
    }

    #[test]
    fn parses_formats() {
        assert_eq!(
            parse(&format!("{}  kiosk.iso.zst\n", A)).unwrap(),
            vec![entry(A, Some("kiosk.iso.zst"))]
        );
        assert_eq!(
            parse(&format!("{} *kiosk.iso.zst", A.to_uppercase())).unwrap(),
            vec![entry(A, Some("kiosk.iso.zst"))]
        );
        assert_eq!(
            parse(&format!("SHA256 (kiosk.iso) = {}", A)).unwrap(),
            vec![entry(A, Some("kiosk.iso"))]
        );
        assert_eq!(parse(&format!("{}\n", A)).unwrap(), vec![entry(A, None)]);
        assert_eq!(
            parse(&format!("# sums\n\n{}  a.img\n{}  b.img\n", A, B))
                .unwrap()
                .len(),
            2
        );
        assert!(parse("deadbeef  short.img").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn selects_by_name() {
        let img = Path::new("/dl/kiosk.iso.zst");
        let sums = || {
            vec![
                entry(A, Some("other.img")),
                entry(B, Some("./kiosk.iso.zst")),
            ]
        };
        let e = select(sums(), img, true, "SUMS").unwrap();
        assert_eq!((e.hex.as_str(), e.covers), (B, Covers::File));

        let inner = vec![entry(A, Some("kiosk.iso"))];
        let e = select(inner, img, true, "SUMS").unwrap();
        assert_eq!((e.hex.as_str(), e.covers), (A, Covers::Decompressed));

        let e = select(vec![entry(A, None)], img, true, "SUMS").unwrap();
        assert_eq!(e.covers, Covers::File);

        assert!(select(vec![entry(A, Some("other.iso"))], img, true, "SUMS").is_err());
        // A raw image never matches the name without .zst.
        let raw = Path::new("kiosk.iso");
        assert!(select(vec![entry(A, Some("kiosk"))], raw, false, "SUMS").is_err());
    }

    #[test]
    fn resolves_sidecar_and_args() {
        let dir = std::env::temp_dir().join(format!("disk-cloner-res-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("kiosk.iso.zst");
        assert!(resolve(None, &img, true).unwrap().is_none());

        std::fs::write(
            dir.join("kiosk.iso.zst.sha256"),
            format!("{}  kiosk.iso.zst\n", A),
        )
        .unwrap();
        let e = resolve(None, &img, true).unwrap().unwrap();
        assert_eq!((e.hex.as_str(), e.covers), (A, Covers::File));

        let e = resolve(Some(B), &img, true).unwrap().unwrap();
        assert_eq!((e.hex.as_str(), e.source.as_str()), (B, "--sha256"));

        let sums = dir.join("SHA256SUMS");
        std::fs::write(&sums, format!("{}  kiosk.iso\n", B)).unwrap();
        let e = resolve(sums.to_str(), &img, true).unwrap().unwrap();
        assert_eq!((e.hex.as_str(), e.covers), (B, Covers::Decompressed));

        assert!(resolve(Some("/nonexistent/SUMS"), &img, true).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hashes_file() {
        let p = std::env::temp_dir().join(format!("disk-cloner-sum-{}", std::process::id()));
        std::fs::write(&p, b"foo\n").unwrap();
        let h = hash_file(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(h, B);
    }
}
