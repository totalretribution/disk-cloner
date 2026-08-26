use anyhow::{bail, Context, Result};
use std::path::Path;

/// A parsed `sfdisk -d` dump. Header lines are preserved verbatim except for
/// `last-lba` (recomputed for the target disk) and `device` (rewritten).
#[derive(Debug, Clone)]
pub struct Ptable {
    /// "gpt" or "dos" (from `label:`).
    pub label: String,
    pub label_id: Option<String>,
    pub first_lba: Option<u64>,
    pub sector_size: u64,
    /// Header lines we don't interpret, kept in order (e.g. `grain:`).
    pub extra_headers: Vec<String>,
    pub parts: Vec<PtPart>,
}

/// One partition line. Everything after `size=` is kept verbatim so type
/// GUIDs, partition UUIDs, names and attrs survive a rewrite untouched.
#[derive(Debug, Clone)]
pub struct PtPart {
    pub number: u32,
    pub start: u64,
    pub size: u64,
    /// Trailing fields, e.g. `type=..., uuid=..., name="EFI"`.
    pub tail: String,
}

impl Ptable {
    pub fn parse(text: &str) -> Result<Self> {
        let mut label = None;
        let mut label_id = None;
        let mut first_lba = None;
        let mut sector_size = 512u64;
        let mut extra_headers = Vec::new();
        let mut parts = Vec::new();

        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            // Partition lines contain " : "; headers are "key: value".
            if let Some((dev, rest)) = split_part_line(t) {
                parts.push(PtPart::parse(dev, rest)?);
                continue;
            }
            let (k, v) = match t.split_once(':') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => continue,
            };
            match k {
                "label" => label = Some(v.to_string()),
                "label-id" => label_id = Some(v.to_string()),
                "first-lba" => first_lba = Some(v.parse().context("parsing first-lba")?),
                "sector-size" => sector_size = v.parse().context("parsing sector-size")?,
                // Recomputed for the new disk; never carried over.
                "last-lba" | "device" | "unit" => {}
                _ => extra_headers.push(t.to_string()),
            }
        }

        let label = label.context("partition dump has no 'label:' line")?;
        if parts.is_empty() {
            bail!("partition dump lists no partitions");
        }
        Ok(Ptable {
            label,
            label_id,
            first_lba,
            sector_size,
            extra_headers,
            parts,
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&s)
    }

    pub fn is_gpt(&self) -> bool {
        self.label.eq_ignore_ascii_case("gpt")
    }

    /// Sectors at the end of the disk that must stay free: the backup GPT
    /// (33 sectors) plus its header. MBR needs nothing reserved.
    pub fn tail_reserve(&self) -> u64 {
        if self.is_gpt() {
            34
        } else {
            0
        }
    }

    /// Render as an sfdisk script targeting `device`.
    pub fn render(&self, device: &str) -> String {
        let mut out = String::new();
        out.push_str(&format!("label: {}\n", self.label));
        if let Some(id) = &self.label_id {
            out.push_str(&format!("label-id: {}\n", id));
        }
        out.push_str(&format!("device: {}\n", device));
        out.push_str("unit: sectors\n");
        if let Some(f) = self.first_lba {
            out.push_str(&format!("first-lba: {}\n", f));
        }
        out.push_str(&format!("sector-size: {}\n", self.sector_size));
        for h in &self.extra_headers {
            out.push_str(h);
            out.push('\n');
        }
        out.push('\n');
        for p in &self.parts {
            out.push_str(&format!(
                "{} : start={}, size={}",
                crate::restore::target_part(device, p.number),
                p.start,
                p.size
            ));
            if !p.tail.is_empty() {
                out.push_str(", ");
                out.push_str(&p.tail);
            }
            out.push('\n');
        }
        out
    }
}

impl PtPart {
    fn parse(dev: &str, rest: &str) -> Result<Self> {
        let number = crate::backup::part_number(dev);
        if number == 0 {
            bail!("cannot read a partition number from '{}'", dev);
        }
        let mut start = None;
        let mut size = None;
        let mut tail_fields = Vec::new();
        for field in split_fields(rest) {
            let f = field.trim();
            if let Some(v) = f.strip_prefix("start=") {
                start = Some(v.trim().parse::<u64>().context("parsing start=")?);
            } else if let Some(v) = f.strip_prefix("size=") {
                size = Some(v.trim().parse::<u64>().context("parsing size=")?);
            } else if !f.is_empty() {
                tail_fields.push(f.to_string());
            }
        }
        Ok(PtPart {
            number,
            start: start.with_context(|| format!("partition {} has no start=", dev))?,
            size: size.with_context(|| format!("partition {} has no size=", dev))?,
            tail: tail_fields.join(", "),
        })
    }
}

/// Split `/dev/sda1 : start=..., size=...` into ("/dev/sda1", "start=...").
fn split_part_line(line: &str) -> Option<(&str, &str)> {
    let (dev, rest) = line.split_once(':')?;
    let dev = dev.trim();
    if !dev.starts_with('/') {
        return None;
    }
    Some((dev, rest.trim()))
}

/// Split on commas that are not inside a double-quoted value (partition names
/// may contain commas, e.g. `name="Basic data, part"`).
fn split_fields(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = "\
label: gpt
label-id: 09C6382D-83E0-43C3-8529-59D1E138F865
device: /dev/sda
unit: sectors
first-lba: 34
last-lba: 976773134
sector-size: 512

/dev/sda1 : start=        2048, size=     1998848, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, uuid=781F1C26-2988-4688-8E6C-AC179306F269
/dev/sda2 : start=     2000896, size=   924405760, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, uuid=E9A4419E-CC4F-415F-B73B-9EB9CC8BEAAC
";

    #[test]
    fn parses_headers_and_parts() {
        let p = Ptable::parse(DUMP).unwrap();
        assert_eq!(p.label, "gpt");
        assert_eq!(p.first_lba, Some(34));
        assert_eq!(p.sector_size, 512);
        assert_eq!(p.parts.len(), 2);
        assert_eq!(p.parts[1].number, 2);
        assert_eq!(p.parts[1].start, 2000896);
        assert_eq!(p.parts[1].size, 924405760);
        assert!(p.parts[1].tail.contains("uuid=E9A4419E"));
    }

    #[test]
    fn render_drops_last_lba_and_retargets() {
        let mut p = Ptable::parse(DUMP).unwrap();
        p.parts[1].size = 1000;
        let out = p.render("/dev/nvme0n1");
        assert!(!out.contains("last-lba"));
        assert!(out.contains("device: /dev/nvme0n1"));
        assert!(out.contains("/dev/nvme0n1p2 : start=2000896, size=1000,"));
        assert!(out.contains("type=C12A7328"));
    }

    #[test]
    fn quoted_names_with_commas_survive() {
        let line = "/dev/sda1 : start=2048, size=100, type=X, name=\"a, b\"";
        let dump = format!("label: gpt\n{}\n", line);
        let p = Ptable::parse(&dump).unwrap();
        assert_eq!(p.parts[0].tail, "type=X, name=\"a, b\"");
    }
}
