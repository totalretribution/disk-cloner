//! The three screens: Back up, Write and Clone. Each one shows its plan from
//! library calls and turns its settings into CLI arguments.

use std::path::{Path, PathBuf};

use eframe::egui::{
    self, Align, Button, CollapsingHeader, Grid, Layout, RichText, TextEdit, Ui, Vec2,
};

use disk_cloner::backup::{choose_cloner, part_number};
use disk_cloner::checksum::{self, Covers, Expected};
use disk_cloner::lsblk::BlkDev;
use disk_cloner::manifest::Manifest;
use disk_cloner::restore::parse_part_sizes;
use disk_cloner::shrink::{self, Projection, Refit, ShrinkOpts};
use disk_cloner::util::{self, human};
use disk_cloner::write::{probe_image, Format, Probe};

use crate::disks::{self, describe, find, holds_now, is_system, mounts, path_and_size, size};
use crate::theme::{self, chip, mono, muted, small, ACCENT, BLUE, DANGER_TEXT, MUTED, OK, WARN};

/// A CLI run one of the screens asks for. Every action runs as root.
pub struct Action {
    pub args: Vec<String>,
    /// Heading of the progress screen.
    pub title: String,
    pub subtitle: String,
    /// Shown in the footer while the job runs.
    pub stop_warning: String,
}

/// The footer of a screen: a hint and the one button that starts the job.
pub struct Footer {
    pub hint: String,
    pub label: String,
    pub danger: bool,
    /// Err is the reason the button is disabled.
    pub action: Result<Action, String>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Role {
    Source,
    Target,
}

// ---------------------------------------------------------------------------
// Shared pieces

fn header(ui: &mut Ui, title: &str, sub: &str) {
    ui.label(RichText::new(title).size(26.0));
    ui.label(muted(sub));
    ui.add_space(12.0);
}

/// Two cards side by side with an arrow between them.
fn card_row(ui: &mut Ui, left: impl FnOnce(&mut Ui), right: impl FnOnce(&mut Ui)) {
    let arrow_w = 28.0;
    let gap = ui.spacing().item_spacing.x;
    let w = ((ui.available_width() - arrow_w - 2.0 * gap) / 2.0).floor();
    ui.horizontal_top(|ui| {
        card_cell(ui, w, left);
        ui.add_sized(
            [arrow_w, 150.0],
            egui::Label::new(RichText::new(theme::ICON_ARROW).color(ACCENT).size(22.0)),
        );
        card_cell(ui, w, right);
    });
}

fn card_cell(ui: &mut Ui, w: f32, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(Vec2::new(w, 0.0), Layout::top_down(Align::Min), |ui| {
        theme::card().show(ui, |ui| {
            ui.set_width(w - 34.0);
            ui.set_min_height(150.0);
            add(ui);
        });
    });
}

/// A full-width card with a title row.
fn section(ui: &mut Ui, title: &str, right: impl FnOnce(&mut Ui), body: impl FnOnce(&mut Ui)) {
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).size(16.0));
            ui.with_layout(Layout::right_to_left(Align::Center), right);
        });
        body(ui);
    });
}

fn card_header(ui: &mut Ui, label: &str, right: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.label(muted(label).size(13.0));
        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
}

fn identity(ui: &mut Ui, icon: &str, color: egui::Color32, name: &str, path: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).size(28.0).color(color));
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(name).size(18.0));
            ui.label(mono(path).color(MUTED));
        });
    });
}

fn drive_menu(
    ui: &mut Ui,
    selected: &mut String,
    list: &[BlkDev],
    exclude: &str,
    allow_system: bool,
) {
    ui.menu_button("Change…", |ui| {
        for d in disks::candidates(list) {
            let path = d.dev_path();
            if path == exclude {
                continue;
            }
            let text = format!("{}   {}   {}", path, human(size(d)), describe(d));
            let enabled = allow_system || !is_system(d);
            if ui
                .add_enabled(enabled, Button::selectable(*selected == path, text))
                .clicked()
            {
                *selected = path;
                ui.close();
            }
        }
    });
}

fn drive_card(
    ui: &mut Ui,
    label: &str,
    selected: &mut String,
    list: &[BlkDev],
    role: Role,
    exclude: &str,
) {
    card_header(ui, label, |ui| {
        drive_menu(ui, selected, list, exclude, role == Role::Source)
    });
    let Some(d) = find(list, selected) else {
        ui.add_space(12.0);
        ui.label(muted("Pick a drive in the sidebar."));
        return;
    };
    let color = if role == Role::Target {
        DANGER_TEXT
    } else {
        ACCENT
    };
    identity(
        ui,
        theme::ICON_DRIVE,
        color,
        &describe(d),
        &path_and_size(d),
    );
    ui.horizontal_wrapped(|ui| {
        match role {
            Role::Target => chip(ui, "Will be erased", DANGER_TEXT),
            Role::Source => chip(ui, "Only read", OK),
        }
        if is_system(d) {
            chip(ui, "System drive", DANGER_TEXT);
        } else if !mounts(d).is_empty() {
            chip(ui, "Mounted", WARN);
        } else {
            chip(ui, "Not mounted", MUTED);
        }
    });
    if role == Role::Target {
        ui.label(small(holds_now(d)));
    }
}

/// Warn when a chosen drive has mounted filesystems.
fn mount_warning(ui: &mut Ui, list: &[BlkDev], path: &str, role: Role, force: &mut bool) {
    let Some(d) = find(list, path) else {
        return;
    };
    let m = mounts(d);
    if m.is_empty() {
        return;
    }
    let (color, title) = if is_system(d) {
        (DANGER_TEXT, format!("{} holds the running system", path))
    } else {
        (WARN, format!("{} is mounted", path))
    };
    theme::callout(ui, color, theme::ICON_WARN, &title, |ui| {
        ui.label(format!("Mounted on {}. Unmount it first.", m.join(", ")));
        let what = match role {
            Role::Source => "Use it anyway (the copy may be inconsistent)",
            Role::Target => "Use it anyway (dangerous)",
        };
        ui.checkbox(force, what);
    });
}

fn table(ui: &mut Ui, id: &str, headers: &[&str], rows: Vec<Vec<RichText>>) {
    Grid::new(id)
        .num_columns(headers.len())
        .spacing([28.0, 8.0])
        .striped(true)
        .show(ui, |ui| {
            for h in headers {
                ui.label(small(*h));
            }
            ui.end_row();
            for row in rows {
                for cell in row {
                    ui.label(cell);
                }
                ui.end_row();
            }
        });
}

/// Legend entries for the filesystems in a list, each once.
fn fs_legend<'a>(fstypes: impl Iterator<Item = Option<&'a str>>) -> Vec<(String, egui::Color32)> {
    let mut out: Vec<(String, egui::Color32)> = Vec::new();
    for fs in fstypes {
        let name = fs.unwrap_or("other").to_string();
        if !out.iter().any(|(n, _)| *n == name) {
            out.push((name, theme::fs_color(fs)));
        }
    }
    out
}

fn bar_with_caption(ui: &mut Ui, caption: &str, segments: &[(u64, egui::Color32)], total: u64) {
    ui.horizontal(|ui| {
        ui.label(small(caption));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(mono(human(total)).color(MUTED).size(12.0));
        });
    });
    theme::segment_bar(ui, segments, total, 20.0);
}

/// One partition of a source drive and how it will be copied.
#[derive(Clone)]
struct SourcePart {
    number: u32,
    fstype: Option<String>,
    label: String,
    bytes: u64,
    cloner: String,
}

/// Cloner choice per partition. `choose_cloner` probes PATH, so this is
/// cached per drive rather than run every frame.
#[derive(Default)]
struct SourceParts {
    key: String,
    parts: Vec<SourcePart>,
}

impl SourceParts {
    fn get(&mut self, d: &BlkDev) -> &[SourcePart] {
        if self.key != d.dev_path() {
            self.key = d.dev_path();
            self.parts = disks::partitions(d)
                .map(|p| SourcePart {
                    number: part_number(&p.name),
                    fstype: p.fstype.clone(),
                    label: p.label.clone().unwrap_or_default(),
                    bytes: size(p),
                    cloner: choose_cloner(p.fstype.as_deref())
                        .unwrap_or_else(|_| "partclone.dd".into()),
                })
                .collect();
        }
        &self.parts
    }
}

fn source_table(ui: &mut Ui, id: &str, parts: &[SourcePart], last_col: &str, swap_note: &str) {
    let rows = parts
        .iter()
        .map(|p| {
            let how = match p.cloner.as_str() {
                "mkswap" => swap_note.to_string(),
                "partclone.dd" => "partclone.dd, every block (filesystem not supported)".into(),
                c => format!("{}, used blocks only", c),
            };
            vec![
                mono(p.number.to_string()),
                RichText::new(p.fstype.as_deref().unwrap_or("unformatted"))
                    .color(theme::fs_color(p.fstype.as_deref())),
                RichText::new(&p.label),
                mono(human(p.bytes)),
                muted(how),
            ]
        })
        .collect();
    table(
        ui,
        id,
        &["#", "Filesystem", "Label", "Size", last_col],
        rows,
    );
}

fn source_bar(ui: &mut Ui, d: &BlkDev, parts: &[SourcePart]) {
    let segments: Vec<_> = parts
        .iter()
        .map(|p| (p.bytes, theme::fs_color(p.fstype.as_deref())))
        .collect();
    theme::segment_bar(ui, &segments, size(d), 20.0);
}

fn pick_folder(start: &str) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new();
    if let Some(dir) = existing_ancestor(Path::new(start)) {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder()
}

fn pick_file(start: Option<&Path>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new();
    if let Some(dir) = start.and_then(existing_ancestor) {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_file()
}

fn existing_ancestor(p: &Path) -> Option<PathBuf> {
    p.ancestors()
        .find(|a| !a.as_os_str().is_empty() && a.is_dir())
        .map(Path::to_path_buf)
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn flag(args: &mut Vec<String>, on: bool, name: &str) {
    if on {
        args.push(name.to_string());
    }
}

// ---------------------------------------------------------------------------
// Back up

#[derive(Clone, Copy, PartialEq)]
enum Compression {
    Zstd,
    Gzip,
    None,
}

pub struct BackupForm {
    pub source: String,
    output: String,
    compression: Compression,
    fsck: bool,
    overwrite: bool,
    force: bool,
    parts: SourceParts,
    /// Free space where the backup goes, cached by folder.
    free: (String, Option<u64>),
}

impl Default for BackupForm {
    fn default() -> Self {
        BackupForm {
            source: String::new(),
            output: String::new(),
            compression: Compression::Zstd,
            fsck: false,
            overwrite: false,
            force: false,
            parts: SourceParts::default(),
            free: (String::new(), None),
        }
    }
}

impl BackupForm {
    pub fn set_output(&mut self, p: &Path) {
        self.output = p.display().to_string();
    }

    fn holds_backup(&self) -> bool {
        !self.output.is_empty() && Path::new(&self.output).join("manifest.json").is_file()
    }

    pub fn ui(&mut self, ui: &mut Ui, list: &[BlkDev]) {
        header(
            ui,
            "Back up a drive",
            "Saves the partition table and an image of every partition into a folder. Only used blocks are copied.",
        );
        let holds_backup = self.holds_backup();
        if self.free.0 != self.output {
            self.free = (
                self.output.clone(),
                existing_ancestor(Path::new(&self.output))
                    .and_then(|a| util::fs_free_bytes(&a).ok()),
            );
        }
        let free = self.free.1;
        card_row(
            ui,
            |ui| drive_card(ui, "Drive", &mut self.source, list, Role::Source, ""),
            |ui| {
                card_header(ui, "Save to", |ui| {
                    if ui.button("Change…").clicked() {
                        if let Some(p) = pick_folder(&self.output) {
                            self.output = p.display().to_string();
                        }
                    }
                });
                if self.output.is_empty() {
                    ui.add_space(12.0);
                    ui.label(muted("Choose a folder for the backup."));
                    return;
                }
                let p = PathBuf::from(&self.output);
                identity(ui, theme::ICON_FOLDER, ACCENT, &file_name(&p), &self.output);
                ui.horizontal_wrapped(|ui| {
                    if holds_backup {
                        chip(ui, "Holds a backup", WARN);
                    } else if p.is_dir() {
                        chip(ui, "Existing folder", MUTED);
                    } else {
                        chip(ui, "New folder", MUTED);
                    }
                    if let Some(f) = free {
                        chip(ui, &format!("{} free", human(f)), MUTED);
                    }
                });
            },
        );
        mount_warning(ui, list, &self.source, Role::Source, &mut self.force);

        if let Some(d) = find(list, &self.source) {
            let parts = self.parts.get(d).to_vec();
            section(
                ui,
                "What gets saved",
                |ui| theme::legend(ui, &fs_legend(parts.iter().map(|p| p.fstype.as_deref()))),
                |ui| {
                    source_bar(ui, d, &parts);
                    source_table(
                        ui,
                        "backup-parts",
                        &parts,
                        "Saved as",
                        "Not saved, recreated on write",
                    );
                },
            );
        }

        ui.columns(2, |cols| {
            theme::card().show(&mut cols[0], |ui| {
                ui.set_width(ui.available_width());
                ui.label(muted("Compression").size(13.0));
                ui.horizontal(|ui| {
                    for (c, name) in [
                        (Compression::Zstd, "zstd"),
                        (Compression::Gzip, "gzip"),
                        (Compression::None, "None"),
                    ] {
                        let on = self.compression == c;
                        let b = Button::new(RichText::new(name).color(if on { theme::TEXT } else { MUTED }))
                            .fill(if on { theme::tint(ACCENT, 0.15) } else { egui::Color32::TRANSPARENT })
                            .stroke(egui::Stroke::new(1.0, if on { theme::tint(ACCENT, 0.5) } else { theme::BORDER }))
                            .min_size(Vec2::new(72.0, 34.0));
                        if ui.add(b).clicked() {
                            self.compression = c;
                        }
                    }
                });
                ui.label(small(match self.compression {
                    Compression::Zstd => "Fast and small. The usual choice.",
                    Compression::Gzip => "Slower than zstd, for older tools.",
                    Compression::None => "Largest backup, fastest to make.",
                }));
            });
            theme::card().show(&mut cols[1], |ui| {
                ui.set_width(ui.available_width());
                ui.label(muted("Before imaging").size(13.0));
                ui.checkbox(&mut self.fsck, "Repair filesystems (fsck)");
                ui.label(
                    RichText::new("Writes to the source drive. Use when partclone refuses a dirty filesystem.")
                        .color(WARN)
                        .size(12.0),
                );
                ui.checkbox(&mut self.overwrite, "Overwrite a backup already in the folder");
            });
        });
    }

    pub fn footer(&self, list: &[BlkDev]) -> Footer {
        Footer {
            hint: if self.fsck {
                "fsck writes to the source drive before it is imaged.".into()
            } else {
                "The source drive is only read. It must not be mounted while imaging.".into()
            },
            label: "Start backup".into(),
            danger: false,
            action: self.action(list),
        }
    }

    fn action(&self, list: &[BlkDev]) -> Result<Action, String> {
        let d = find(list, &self.source).ok_or("Choose the drive to back up.")?;
        if self.output.is_empty() {
            return Err("Choose a folder for the backup.".into());
        }
        if self.holds_backup() && !self.overwrite {
            return Err(
                "The folder already holds a backup. Tick overwrite or pick another folder.".into(),
            );
        }
        let mut args = vec![
            "backup".into(),
            d.dev_path(),
            "-o".into(),
            self.output.clone(),
            "-c".into(),
            match self.compression {
                Compression::Zstd => "zstd",
                Compression::Gzip => "gzip",
                Compression::None => "none",
            }
            .into(),
        ];
        flag(&mut args, self.fsck, "--fsck");
        flag(&mut args, self.overwrite, "--overwrite");
        flag(&mut args, self.force, "--force");
        Ok(Action {
            args,
            title: format!("Backing up {}", d.dev_path()),
            subtitle: format!("{} to {}", describe(d), self.output),
            stop_warning: "Stopping now leaves an incomplete backup in the folder.".into(),
        })
    }
}

// ---------------------------------------------------------------------------
// Write

/// What the chosen image turned out to be.
enum Image {
    /// A disk-cloner backup folder.
    Backup(Manifest),
    /// A raw disk image, possibly zstd-compressed.
    Raw(Probe),
}

#[derive(Clone, PartialEq)]
enum ChecksumChoice {
    /// `<image>.sha256` if it exists.
    Auto,
    File(PathBuf),
    Hash(String),
    Skip,
}

pub struct WriteForm {
    image_path: Option<PathBuf>,
    image: Option<Result<Image, String>>,
    pub device: String,
    refit: bool,
    /// The image and drive the refit default was chosen for.
    refit_for: String,
    swap_size: String,
    part_sizes: String,
    scratch: String,
    keep_scratch: bool,
    skip_ptable: bool,
    force: bool,
    projection: Option<Result<Projection, String>>,
    projection_key: String,
    scratch_free: (String, Option<u64>),
    checksum: ChecksumChoice,
    hash_input: String,
    editing_hash: bool,
    expected: Option<Result<Option<Expected>, String>>,
    expected_key: (Option<PathBuf>, Option<ChecksumChoice>),
}

impl Default for WriteForm {
    fn default() -> Self {
        WriteForm {
            image_path: None,
            image: None,
            device: String::new(),
            refit: false,
            refit_for: String::new(),
            swap_size: String::new(),
            part_sizes: String::new(),
            scratch: "/var/tmp".into(),
            keep_scratch: false,
            skip_ptable: false,
            force: false,
            projection: None,
            projection_key: String::new(),
            scratch_free: (String::new(), None),
            checksum: ChecksumChoice::Auto,
            hash_input: String::new(),
            editing_hash: false,
            expected: None,
            expected_key: (None, None),
        }
    }
}

impl WriteForm {
    /// Use a backup folder or an image file, detected from what it holds.
    pub fn set_image(&mut self, p: &Path) {
        let p = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        self.image = Some(if p.is_dir() {
            if p.join("manifest.json").is_file() {
                Manifest::load(&p)
                    .map(Image::Backup)
                    .map_err(|e| format!("{:#}", e))
            } else {
                Err("This folder is not a disk-cloner backup (no manifest.json).".into())
            }
        } else {
            probe_image(&p)
                .map(Image::Raw)
                .map_err(|e| format!("{:#}", e))
        });
        self.image_path = Some(p);
        self.checksum = ChecksumChoice::Auto;
        self.editing_hash = false;
    }

    /// Recompute what depends on the image and drive, when they change.
    fn refresh(&mut self, list: &[BlkDev]) {
        let target = find(list, &self.device);
        let path = self.image_path.clone().unwrap_or_default();
        let key = format!("{}|{}", path.display(), self.device);
        if let (Some(Ok(Image::Backup(m))), Some(d)) = (&self.image, target) {
            if self.refit_for != key {
                self.refit_for = key.clone();
                self.refit = size(d) < m.disk_size_bytes;
            }
        }

        let pkey = format!(
            "{}|{}|{}|{}|{}",
            key, self.refit, self.swap_size, self.part_sizes, self.scratch
        );
        if pkey != self.projection_key {
            self.projection_key = pkey;
            self.projection = match (&self.image, target) {
                (Some(Ok(Image::Backup(m))), Some(d)) if self.refit => {
                    Some(self.project(&path, m, d))
                }
                _ => None,
            };
        }
        if self.scratch_free.0 != self.scratch {
            self.scratch_free = (
                self.scratch.clone(),
                existing_ancestor(Path::new(&self.scratch))
                    .and_then(|a| util::fs_free_bytes(&a).ok()),
            );
        }

        let ekey = (self.image_path.clone(), Some(self.checksum.clone()));
        if ekey != self.expected_key {
            self.expected_key = ekey;
            self.expected = match &self.image {
                Some(Ok(Image::Raw(probe))) => {
                    let zstd = probe.format == Format::Zstd;
                    let arg = match &self.checksum {
                        ChecksumChoice::Auto => None,
                        ChecksumChoice::File(f) => Some(f.display().to_string()),
                        ChecksumChoice::Hash(h) => Some(h.clone()),
                        ChecksumChoice::Skip => {
                            self.expected = None;
                            return;
                        }
                    };
                    Some(
                        checksum::resolve(arg.as_deref(), &path, zstd)
                            .map_err(|e| format!("{:#}", e)),
                    )
                }
                _ => None,
            };
        }
    }

    fn opts(&self) -> Result<ShrinkOpts, String> {
        let swap_size = match self.swap_size.trim() {
            "" => None,
            s => Some(util::parse_size(s).map_err(|e| format!("Swap size: {:#}", e))?),
        };
        let pins: Vec<String> = self
            .part_sizes
            .split([',', ' '])
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect();
        Ok(ShrinkOpts {
            swap_size,
            part_size: parse_part_sizes(&pins).map_err(|e| format!("Pinned sizes: {:#}", e))?,
            scratch: PathBuf::from(&self.scratch),
            keep_scratch: self.keep_scratch,
            dry_run: true,
            yes: false,
        })
    }

    fn project(&self, dir: &Path, m: &Manifest, d: &BlkDev) -> Result<Projection, String> {
        let opts = self.opts()?;
        shrink::project(
            dir,
            m,
            &d.dev_path(),
            size(d),
            d.log_sec.unwrap_or(512),
            &opts,
        )
        .map_err(|e| format!("{:#}", e))
    }

    pub fn ui(&mut self, ui: &mut Ui, list: &[BlkDev]) {
        self.refresh(list);
        header(
            ui,
            "Write an image to a drive",
            "Restore a disk-cloner backup, or write an .iso, .img or .zst. The kind of image is detected from the file.",
        );
        let mut picked: Option<PathBuf> = None;
        card_row(
            ui,
            |ui| {
                image_card(
                    ui,
                    self.image_path.as_deref(),
                    self.image.as_ref(),
                    &mut picked,
                )
            },
            |ui| drive_card(ui, "Drive", &mut self.device, list, Role::Target, ""),
        );
        if let Some(p) = picked {
            self.set_image(&p);
            self.refresh(list);
        }
        mount_warning(ui, list, &self.device, Role::Target, &mut self.force);

        let target = find(list, &self.device);
        match &self.image {
            Some(Ok(Image::Backup(_))) => self.backup_plan(ui, target),
            Some(Ok(Image::Raw(_))) => self.raw_plan(ui, target),
            _ => {}
        }
    }
}

fn image_card(
    ui: &mut Ui,
    image_path: Option<&Path>,
    image: Option<&Result<Image, String>>,
    picked: &mut Option<PathBuf>,
) {
    let start = image_path;
    card_header(ui, "Image", |ui| {
        ui.menu_button("Change…", |ui| {
            if ui.button("Backup folder…").clicked() {
                *picked = pick_folder(&start.map(|p| p.display().to_string()).unwrap_or_default());
                ui.close();
            }
            if ui.button("Image file (.iso, .img, .zst)…").clicked() {
                *picked = pick_file(start);
                ui.close();
            }
        });
    });
    let Some(path) = image_path else {
        ui.add_space(12.0);
        ui.label(muted("Choose a backup folder or an image file,"));
        ui.label(muted("or drop one onto the window."));
        return;
    };
    let parent = path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    match image {
        Some(Ok(Image::Backup(m))) => {
            identity(
                ui,
                theme::ICON_FOLDER,
                ACCENT,
                &file_name(path),
                &path.display().to_string(),
            );
            ui.horizontal_wrapped(|ui| {
                chip(ui, "disk-cloner backup", ACCENT);
                chip(ui, &format!("{} partitions", m.partitions.len()), MUTED);
                chip(ui, &m.compression, MUTED);
            });
            ui.label(small(format!(
                "Taken {} from {}, {}",
                taken(&m.created_utc),
                m.source_device,
                human(m.disk_size_bytes)
            )));
        }
        Some(Ok(Image::Raw(p))) => {
            identity(ui, theme::ICON_FILE, ACCENT, &file_name(path), &parent);
            ui.horizontal_wrapped(|ui| {
                chip(ui, "Raw disk image", BLUE);
                if p.format == Format::Zstd {
                    chip(ui, "zstd", MUTED);
                }
            });
            ui.label(small(match (p.format, p.raw_size) {
                (Format::Raw, _) => human(p.file_size),
                (Format::Zstd, Some(r)) => {
                    format!("{} on disk, {} unpacked", human(p.file_size), human(r))
                }
                (Format::Zstd, None) => format!(
                    "{} on disk, unpacked size measured before writing",
                    human(p.file_size)
                ),
            }));
        }
        Some(Err(e)) => {
            identity(ui, theme::ICON_FILE, DANGER_TEXT, &file_name(path), &parent);
            ui.label(RichText::new(e).color(DANGER_TEXT));
        }
        None => {}
    }
}

impl WriteForm {
    fn backup_plan(&mut self, ui: &mut Ui, target: Option<&BlkDev>) {
        let Some(Ok(Image::Backup(m))) = &self.image else {
            return;
        };
        let Some(d) = target else {
            return;
        };
        let target_bytes = size(d);
        if target_bytes < m.disk_size_bytes {
            theme::callout(
                ui,
                WARN,
                theme::ICON_WARN,
                "The drive is smaller than the backup",
                |ui| {
                    ui.label(format!(
                    "{} against {}. ext partitions are shrunk to fit and swap is recreated smaller.",
                    human(target_bytes),
                    human(m.disk_size_bytes)
                ));
                    ui.checkbox(&mut self.refit, "Refit the layout to this drive");
                },
            );
        }
        if let Some(Err(e)) = &self.projection {
            theme::callout(
                ui,
                DANGER_TEXT,
                theme::ICON_WARN,
                "This backup cannot be refitted onto the drive",
                |ui| {
                    ui.label(e.as_str());
                },
            );
        }

        let legend = fs_legend(m.partitions.iter().map(|p| p.fstype.as_deref()));
        let projection = match &self.projection {
            Some(Ok(p)) => Some(p),
            _ => None,
        };
        let refit = self.refit;
        let mut grow = refit;
        section(
            ui,
            "Partition layout",
            |ui| theme::legend(ui, &legend),
            |ui| {
                if target_bytes >= m.disk_size_bytes {
                    ui.checkbox(
                        &mut grow,
                        "Refit the layout to this drive (grows ext partitions to fill it)",
                    );
                }
                if refit {
                    ui.label(small(
                        "Projected. Measured exactly before anything is written.",
                    ));
                }
                let before: Vec<_> = m
                    .partitions
                    .iter()
                    .map(|p| (p.size_bytes, theme::fs_color(p.fstype.as_deref())))
                    .collect();
                bar_with_caption(ui, "In the backup", &before, m.disk_size_bytes);
                let after: Vec<_> = match projection {
                    Some(pr) => pr
                        .parts
                        .iter()
                        .map(|p| {
                            let fs = (p.fstype != "unimaged").then_some(p.fstype.as_str());
                            (p.new_bytes.unwrap_or(p.orig_bytes), theme::fs_color(fs))
                        })
                        .collect(),
                    None if refit => Vec::new(),
                    None => before.clone(),
                };
                bar_with_caption(
                    ui,
                    &format!("On {} after writing", d.dev_path()),
                    &after,
                    target_bytes,
                );

                let rows: Vec<Vec<RichText>> = match projection {
                    Some(pr) => pr
                        .parts
                        .iter()
                        .map(|p| {
                            let changed = p.new_bytes != Some(p.orig_bytes);
                            let new = p
                                .new_bytes
                                .map(human)
                                .unwrap_or_else(|| "measured later".into());
                            let how = match p.refit {
                                Refit::Unchanged => {
                                    format!("Copied as is ({} cannot resize)", p.fstype)
                                }
                                Refit::Pinned => "Pinned to the size you set".into(),
                                Refit::Recreated => "Recreated with mkswap, same UUID".into(),
                                Refit::Resized => match (p.new_bytes, p.used_bytes) {
                                    (Some(n), _) if n > p.orig_bytes => {
                                        "Grown to fill the drive".into()
                                    }
                                    (_, Some(u)) => format!("Shrunk to fit, {} in use", human(u)),
                                    _ => "Shrunk to fit".into(),
                                },
                            };
                            vec![
                                mono(p.number.to_string()),
                                RichText::new(&p.fstype).color(theme::fs_color(Some(&p.fstype))),
                                mono(human(p.orig_bytes)),
                                mono(new).color(if changed { ACCENT } else { theme::TEXT }),
                                muted(how),
                            ]
                        })
                        .collect(),
                    None => m
                        .partitions
                        .iter()
                        .map(|p| {
                            let fs = p.fstype.clone().unwrap_or_else(|| "unformatted".into());
                            let how = if p.cloner == "mkswap" {
                                "Recreated with mkswap, same UUID"
                            } else {
                                "Restored at its original size"
                            };
                            vec![
                                mono(p.number.to_string()),
                                RichText::new(&fs).color(theme::fs_color(Some(&fs))),
                                mono(human(p.size_bytes)),
                                mono(human(p.size_bytes)),
                                muted(how),
                            ]
                        })
                        .collect(),
                };
                table(
                    ui,
                    "write-parts",
                    &["#", "Filesystem", "In backup", "On drive", "What happens"],
                    rows,
                );

                CollapsingHeader::new(muted("Advanced"))
                    .id_salt("write-advanced")
                    .show(ui, |ui| {
                        if refit {
                            Grid::new("refit-opts").spacing([12.0, 8.0]).show(ui, |ui| {
                                ui.label("Swap size");
                                ui.add(
                                    TextEdit::singleline(&mut self.swap_size)
                                        .hint_text("auto (e.g. 4G, 0 drops it)")
                                        .desired_width(220.0),
                                );
                                ui.end_row();
                                ui.label("Pin sizes");
                                ui.add(
                                    TextEdit::singleline(&mut self.part_sizes)
                                        .hint_text("e.g. 2=40G 3=10G")
                                        .desired_width(220.0),
                                );
                                ui.end_row();
                                ui.label("Scratch space");
                                ui.horizontal(|ui| {
                                    ui.label(mono(&self.scratch));
                                    if ui.button("Change…").clicked() {
                                        if let Some(p) = pick_folder(&self.scratch) {
                                            self.scratch = p.display().to_string();
                                        }
                                    }
                                });
                                ui.end_row();
                            });
                            if let Some(pr) = projection {
                                let free = self.scratch_free.1.unwrap_or(0);
                                let text = format!(
                                    "Staging needs {}, {} free.",
                                    human(pr.scratch_need),
                                    human(free)
                                );
                                let color = if free >= pr.scratch_need {
                                    MUTED
                                } else {
                                    DANGER_TEXT
                                };
                                ui.label(RichText::new(text).color(color).size(12.0));
                            }
                            ui.checkbox(&mut self.keep_scratch, "Keep staging files afterwards");
                        } else {
                            ui.checkbox(&mut self.skip_ptable, "Keep the drive's partition table");
                        }
                        ui.checkbox(&mut self.force, "Write even if the drive is mounted");
                    });
            },
        );
        self.refit = grow;
    }

    fn raw_plan(&mut self, ui: &mut Ui, target: Option<&BlkDev>) {
        self.checksum_notice(ui);
        let Some(Ok(Image::Raw(p))) = &self.image else {
            return;
        };
        let Some(d) = target else {
            return;
        };
        let total = size(d);
        if let Some(raw) = p.raw_size.filter(|r| *r > total) {
            theme::callout(
                ui,
                DANGER_TEXT,
                theme::ICON_WARN,
                "The image does not fit",
                |ui| {
                    ui.label(format!(
                        "It needs {} but {} holds {}.",
                        human(raw),
                        d.dev_path(),
                        human(total)
                    ));
                },
            );
        }
        let raw = p.raw_size;
        section(
            ui,
            &format!("What ends up on {}", d.dev_path()),
            |ui| {
                theme::legend(
                    ui,
                    &[
                        ("unallocated".into(), theme::BORDER),
                        ("image".into(), BLUE),
                    ],
                )
            },
            |ui| {
                theme::segment_bar(ui, &[(raw.unwrap_or(0), BLUE)], total, 20.0);
                ui.horizontal(|ui| {
                    ui.label(
                        mono(match raw {
                            Some(r) => format!("image {}", human(r)),
                            None => "image size known after unpacking".into(),
                        })
                        .color(MUTED)
                        .size(12.0),
                    );
                    if let Some(r) = raw.filter(|r| *r <= total) {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(
                                mono(format!("unallocated {}", human(total - r)))
                                    .color(MUTED)
                                    .size(12.0),
                            );
                        });
                    }
                });
                ui.label(muted("The image brings its own partition table, which replaces the drive's. The rest of the drive is left unallocated.").size(13.0));
                CollapsingHeader::new(muted("Advanced"))
                    .id_salt("raw-advanced")
                    .show(ui, |ui| {
                        ui.checkbox(
                            &mut self.force,
                            "Write even if the drive is mounted or in use",
                        );
                    });
            },
        );
    }

    fn checksum_notice(&mut self, ui: &mut Ui) {
        let (color, icon, title, lines): (_, _, String, Vec<RichText>) = match (
            &self.checksum,
            &self.expected,
        ) {
            (ChecksumChoice::Skip, _) => (
                MUTED,
                theme::ICON_WARN,
                "Checksum check skipped".into(),
                vec![muted("Nothing is verified.")],
            ),
            (_, Some(Ok(Some(e)))) => {
                let covers = match e.covers {
                    Covers::File => "Covers the image file. Checked before writing.",
                    Covers::Decompressed => "Covers the unpacked image. Checked before writing and again against what lands on the drive.",
                };
                let source = if e.source == "--sha256" {
                    "the hash you entered".to_string()
                } else {
                    e.source.clone()
                };
                (
                    OK,
                    theme::ICON_CHECK,
                    "SHA-256 found".into(),
                    vec![mono(source), muted(covers).size(13.0)],
                )
            }
            (_, Some(Ok(None))) => (
                MUTED,
                theme::ICON_WARN,
                "No checksum next to the image".into(),
                vec![muted(
                    "Nothing will be verified. Add one to catch a corrupt download.",
                )],
            ),
            (_, Some(Err(e))) => (
                DANGER_TEXT,
                theme::ICON_WARN,
                "Checksum problem".into(),
                vec![RichText::new(e)],
            ),
            (_, None) => return,
        };
        let mut choice = None;
        theme::callout(ui, color, icon, &title, |ui| {
            for l in lines {
                ui.label(l);
            }
            if self.editing_hash {
                ui.horizontal(|ui| {
                    ui.add(
                        TextEdit::singleline(&mut self.hash_input)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("sha256 hex")
                            .desired_width(440.0),
                    );
                    if ui.button("Use").clicked() {
                        choice = Some(ChecksumChoice::Hash(self.hash_input.trim().to_string()));
                    }
                });
            }
            ui.menu_button("Use another…", |ui| {
                if ui.button("Checksum file…").clicked() {
                    choice = pick_file(self.image_path.as_deref()).map(ChecksumChoice::File);
                    ui.close();
                }
                if ui.button("Enter a hash…").clicked() {
                    self.editing_hash = true;
                    ui.close();
                }
                if ui.button("Look next to the image").clicked() {
                    choice = Some(ChecksumChoice::Auto);
                    ui.close();
                }
                if ui.button("Don't check").clicked() {
                    choice = Some(ChecksumChoice::Skip);
                    ui.close();
                }
            });
        });
        if let Some(c) = choice {
            self.checksum = c;
            self.editing_hash = false;
        }
    }

    pub fn footer(&self, list: &[BlkDev]) -> Footer {
        let label = match find(list, &self.device) {
            Some(d) => format!("Erase {} and write…", d.dev_path()),
            None => "Erase and write…".into(),
        };
        Footer {
            hint: "Nothing is written until you confirm in the next step.".into(),
            label,
            danger: true,
            action: self.action(list),
        }
    }

    fn action(&self, list: &[BlkDev]) -> Result<Action, String> {
        let path = self.image_path.as_ref().ok_or("Choose an image.")?;
        let image = match &self.image {
            Some(Ok(i)) => i,
            Some(Err(_)) => return Err("This image cannot be written.".into()),
            None => return Err("Choose an image.".into()),
        };
        let d = find(list, &self.device).ok_or("Choose the drive to write to.")?;
        if is_system(d) && !self.force {
            return Err("That drive holds the running system.".into());
        }
        let dev = d.dev_path();
        let name = file_name(path);
        let mut args;
        let subtitle;
        match image {
            Image::Backup(m) => {
                if self.refit {
                    match &self.projection {
                        Some(Ok(_)) => {}
                        _ => return Err("The refit has a problem; see above.".into()),
                    }
                } else if size(d) < m.disk_size_bytes && !self.skip_ptable {
                    return Err("The backup does not fit unless the layout is refitted.".into());
                }
                args = vec!["restore".into(), path.display().to_string(), dev.clone()];
                if self.refit {
                    args.push("--shrink".into());
                    if !self.swap_size.trim().is_empty() {
                        args.extend(["--swap-size".into(), self.swap_size.trim().into()]);
                    }
                    for p in self.part_sizes.split([',', ' ']).filter(|p| !p.is_empty()) {
                        args.extend(["--part-size".into(), p.into()]);
                    }
                    args.extend(["--scratch".into(), self.scratch.clone()]);
                    flag(&mut args, self.keep_scratch, "--keep-scratch");
                    subtitle = format!(
                        "disk-cloner backup, refit from {} to {}",
                        human(m.disk_size_bytes),
                        human(size(d))
                    );
                } else {
                    flag(&mut args, self.skip_ptable, "--skip-ptable");
                    subtitle = "disk-cloner backup".into();
                }
            }
            Image::Raw(p) => {
                if p.raw_size.is_some_and(|r| r > size(d)) {
                    return Err("The image is larger than the drive.".into());
                }
                if let Some(Err(_)) = &self.expected {
                    return Err("Fix the checksum first, or choose not to check.".into());
                }
                args = vec!["write".into(), path.display().to_string(), dev.clone()];
                match &self.checksum {
                    ChecksumChoice::Auto => {}
                    ChecksumChoice::File(f) => {
                        args.extend(["--sha256".into(), f.display().to_string()])
                    }
                    ChecksumChoice::Hash(h) => args.extend(["--sha256".into(), h.clone()]),
                    ChecksumChoice::Skip => args.push("--no-checksum".into()),
                }
                subtitle = if p.format == Format::Zstd {
                    "zstd-compressed disk image".into()
                } else {
                    "Raw disk image".into()
                };
            }
        }
        flag(&mut args, self.force, "--force");
        args.push("--yes".into());
        Ok(Action {
            args,
            title: format!("Writing {} to {}", name, dev),
            subtitle,
            stop_warning: format!(
                "Keep the window open. Stopping now leaves {} unusable until you write it again.",
                dev
            ),
        })
    }
}

/// "2026-06-29T13:01:53Z" -> "2026-06-29 13:01 UTC"
fn taken(created_utc: &str) -> String {
    match (created_utc.get(..10), created_utc.get(11..16)) {
        (Some(d), Some(t)) => format!("{} {} UTC", d, t),
        _ => created_utc.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Clone

#[derive(Default)]
pub struct CloneForm {
    pub source: String,
    pub target: String,
    fsck: bool,
    force: bool,
    parts: SourceParts,
}

impl CloneForm {
    /// A sidebar click fills the source first, then the target.
    pub fn pick(&mut self, path: String) {
        if self.source.is_empty() || self.source == path {
            self.source = path;
        } else {
            self.target = path;
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, list: &[BlkDev]) {
        header(
            ui,
            "Clone a drive",
            "Copies the partition table and every partition straight onto another drive, with no backup in between.",
        );
        let (src, dst) = (self.source.clone(), self.target.clone());
        card_row(
            ui,
            |ui| drive_card(ui, "From", &mut self.source, list, Role::Source, &dst),
            |ui| drive_card(ui, "To", &mut self.target, list, Role::Target, &src),
        );
        mount_warning(ui, list, &self.source, Role::Source, &mut self.force);
        mount_warning(ui, list, &self.target, Role::Target, &mut self.force);

        let (Some(s), t) = (find(list, &self.source), find(list, &self.target)) else {
            return;
        };
        if let Some(t) = t.filter(|t| size(t) < size(s)) {
            theme::callout(
                ui,
                DANGER_TEXT,
                theme::ICON_WARN,
                "The target is smaller than the source",
                |ui| {
                    ui.label(format!(
                    "Clone copies the partition table as it is, so the target needs at least {}. To move onto a smaller drive, back up, then write with refit.",
                    human(size(s))
                ));
                    ui.label(small(format!("{} holds {}.", t.dev_path(), human(size(t)))));
                },
            );
        }
        let parts = self.parts.get(s).to_vec();
        section(
            ui,
            "What gets copied",
            |ui| theme::legend(ui, &fs_legend(parts.iter().map(|p| p.fstype.as_deref()))),
            |ui| {
                source_bar(ui, s, &parts);
                source_table(
                    ui,
                    "clone-parts",
                    &parts,
                    "Copied as",
                    "Recreated with mkswap, same UUID",
                );
            },
        );
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(muted("Before copying").size(13.0));
            ui.checkbox(&mut self.fsck, "Repair source filesystems (fsck)");
            ui.label(
                RichText::new("Writes to the source drive.")
                    .color(WARN)
                    .size(12.0),
            );
        });
    }

    pub fn footer(&self, list: &[BlkDev]) -> Footer {
        let label = match find(list, &self.target) {
            Some(d) => format!("Erase {} and clone…", d.dev_path()),
            None => "Erase and clone…".into(),
        };
        Footer {
            hint: "Nothing is written until you confirm in the next step.".into(),
            label,
            danger: true,
            action: self.action(list),
        }
    }

    fn action(&self, list: &[BlkDev]) -> Result<Action, String> {
        let s = find(list, &self.source).ok_or("Choose the drive to copy from.")?;
        let t = find(list, &self.target).ok_or("Choose the drive to copy onto.")?;
        if s.dev_path() == t.dev_path() {
            return Err("Source and target must be different drives.".into());
        }
        if size(t) < size(s) {
            return Err("The target is smaller than the source.".into());
        }
        if is_system(t) && !self.force {
            return Err("The target holds the running system.".into());
        }
        let mut args = vec!["clone".into(), s.dev_path(), t.dev_path()];
        flag(&mut args, self.fsck, "--fsck");
        flag(&mut args, self.force, "--force");
        args.push("--yes".into());
        Ok(Action {
            args,
            title: format!("Cloning {} to {}", s.dev_path(), t.dev_path()),
            subtitle: format!("{} onto {}", describe(s), describe(t)),
            stop_warning: format!(
                "Keep the window open. Stopping now leaves {} unusable until you clone it again.",
                t.dev_path()
            ),
        })
    }
}
