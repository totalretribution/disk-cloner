//! egui front end for disk-cloner.
//!
//! Read-only information (drives, backup manifests, image headers, refit
//! projections, checksums) comes straight from the library. Anything that
//! reads or writes a drive runs the CLI as a child process; see `runner`.

mod disks;
mod forms;
mod runner;
mod theme;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Button, CollapsingHeader, Frame, Id, Layout, Margin, RichText, ScrollArea, Stroke,
    TextEdit, TextStyle, Ui, Vec2,
};

use disk_cloner::lsblk::{self, BlkDev};
use disk_cloner::util::{self, human};
use disks::{describe, find, is_system, size};
use forms::{Action, BackupForm, CloneForm, WriteForm};
use runner::{Job, Log, Outcome};
use theme::{chip, mono, muted, small, ACCENT, DANGER, DANGER_TEXT, MUTED, OK, TEXT};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Disk Cloner")
            .with_app_id("disk-cloner")
            .with_inner_size([1180.0, 820.0])
            .with_min_inner_size([1000.0, 680.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Disk Cloner",
        options,
        Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))),
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Backup,
    Write,
    Clone,
}

/// The progress screen of the current or last job.
struct JobView {
    title: String,
    subtitle: String,
    stop_warning: String,
    started: Instant,
    ended: Option<Ended>,
}

struct Ended {
    ok: bool,
    after: Duration,
    /// Why it failed.
    message: Option<String>,
}

struct App {
    page: Page,
    disks: Vec<BlkDev>,
    disks_err: Option<String>,
    backup: BackupForm,
    write: WriteForm,
    clone: CloneForm,
    log: Log,
    job: Option<Job>,
    view: Option<JobView>,
    prompt_input: String,
    stop_confirm: bool,
    close_confirm: bool,
    allow_close: bool,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        theme::setup(ctx);
        let mut app = App {
            page: Page::Write,
            disks: Vec::new(),
            disks_err: None,
            backup: BackupForm::default(),
            write: WriteForm::default(),
            clone: CloneForm::default(),
            log: Log::default(),
            job: None,
            view: None,
            prompt_input: String::new(),
            stop_confirm: false,
            close_confirm: false,
            allow_close: false,
        };
        app.refresh_disks();
        app
    }

    fn refresh_disks(&mut self) {
        match lsblk::list_all() {
            Ok(d) => {
                self.disks = d;
                self.disks_err = None;
            }
            Err(e) => self.disks_err = Some(format!("{:#}", e)),
        }
    }

    fn start(&mut self, a: Action, ctx: &egui::Context) {
        self.log.clear();
        self.prompt_input.clear();
        let mut view = JobView {
            title: a.title,
            subtitle: a.subtitle,
            stop_warning: a.stop_warning,
            started: Instant::now(),
            ended: None,
        };
        match Job::spawn(&view.title, &a.args, true, &mut self.log, ctx) {
            Ok(job) => self.job = Some(job),
            Err(e) => {
                view.ended = Some(Ended {
                    ok: false,
                    after: Duration::ZERO,
                    message: Some(e),
                })
            }
        }
        self.view = Some(view);
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        let Some(job) = self.job.as_mut() else {
            return;
        };
        match job.poll(&mut self.log) {
            None => ctx.request_repaint_after(Duration::from_millis(250)),
            Some(outcome) => {
                let ended = match outcome {
                    Outcome::Success => Ended {
                        ok: true,
                        after: job.started.elapsed(),
                        message: None,
                    },
                    Outcome::Failed(e) => Ended {
                        ok: false,
                        after: job.started.elapsed(),
                        message: Some(self.log.error().map(String::from).unwrap_or(e)),
                    },
                };
                if let Some(v) = self.view.as_mut() {
                    v.ended = Some(ended);
                }
                self.job = None;
                self.prompt_input.clear();
                // Partition tables and mounts may have changed.
                self.refresh_disks();
            }
        }
    }

    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && self.job.is_some() && !self.allow_close
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_confirm = true;
        }
    }

    /// A dropped folder or file becomes the image to write, or on the Back up
    /// screen a plain folder becomes where the backup goes.
    fn handle_drop(&mut self, ctx: &egui::Context) {
        if self.job.is_some() {
            return;
        }
        let dropped: Option<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        let Some(p) = dropped else {
            return;
        };
        if self.page == Page::Backup && p.is_dir() && !p.join("manifest.json").is_file() {
            self.backup.set_output(&p);
        } else {
            self.write.set_image(&p);
            self.page = Page::Write;
        }
        self.view = None;
    }

    fn pick_drive(&mut self, path: String) {
        match self.page {
            Page::Backup => self.backup.source = path,
            Page::Write => self.write.device = path,
            Page::Clone => self.clone.pick(path),
        }
    }

    /// The tag a drive carries in the sidebar on this screen.
    fn role_tag(&self, path: &str) -> Option<(&'static str, egui::Color32)> {
        match self.page {
            Page::Backup if self.backup.source == path => Some(("Source", ACCENT)),
            Page::Write if self.write.device == path => Some(("Target", DANGER_TEXT)),
            Page::Clone if self.clone.source == path => Some(("Source", ACCENT)),
            Page::Clone if self.clone.target == path => Some(("Target", DANGER_TEXT)),
            _ => None,
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(RichText::new(theme::ICON_DRIVE).color(ACCENT).size(16.0));
            ui.label(mono("disk-cloner").size(16.0).color(TEXT));
        });
        ui.add_space(14.0);

        let running = self.job.is_some();
        ui.spacing_mut().item_spacing.y = 4.0;
        for (page, icon, name) in [
            (Page::Backup, theme::ICON_BACKUP, "Back up"),
            (Page::Write, theme::ICON_WRITE, "Write"),
            (Page::Clone, theme::ICON_CLONE, "Clone"),
        ] {
            let selected = self.page == page;
            let pct = (selected && running).then(|| self.log.progress()).flatten();
            let r = theme::row_button(
                ui,
                selected,
                theme::Selected::Tint,
                !running || selected,
                Margin::symmetric(12, 9),
                |ui| {
                    ui.horizontal(|ui| {
                        let c = if selected { ACCENT } else { MUTED };
                        ui.add_sized([18.0, 18.0], egui::Label::new(RichText::new(icon).color(c)));
                        ui.label(RichText::new(name).color(if selected { TEXT } else { MUTED }));
                        if let Some(p) = pct {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    mono(format!("{:.0}%", p * 100.0)).color(ACCENT).size(12.0),
                                );
                            });
                        }
                    });
                },
            );
            if r.clicked() {
                self.page = page;
                if !running {
                    self.view = None;
                }
            }
        }

        ui.add_space(18.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.label(muted("Drives").size(13.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let refresh =
                    Button::new(RichText::new(theme::ICON_REFRESH).size(16.0).color(MUTED))
                        .frame(false);
                if ui
                    .add_enabled(!running, refresh)
                    .on_hover_text("Refresh drives")
                    .clicked()
                {
                    self.refresh_disks();
                }
            });
        });
        if let Some(e) = &self.disks_err {
            ui.label(RichText::new(e).color(DANGER_TEXT).size(12.0));
        }

        let mut clicked = None;
        for d in disks::candidates(&self.disks) {
            let path = d.dev_path();
            let tag = self.role_tag(&path);
            let system = is_system(d);
            let enabled = !running && (self.page == Page::Backup || !system);
            let r = theme::row_button(
                ui,
                tag.is_some(),
                theme::Selected::Outline,
                enabled,
                Margin::symmetric(12, 10),
                |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    ui.horizontal(|ui| {
                        ui.label(mono(&path));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(small(human(size(d))));
                        });
                    });
                    ui.horizontal(|ui| {
                        if system {
                            ui.label(small(format!("{} System drive", theme::ICON_LOCK)));
                        } else {
                            ui.label(small(describe(d)));
                        }
                        if let Some((t, c)) = tag {
                            chip(ui, t, c);
                        }
                    });
                    theme::segment_bar(ui, &disks::bar_segments(d), size(d), 4.0);
                },
            );
            if r.clicked() {
                clicked = Some(path);
            }
        }
        if let Some(p) = clicked {
            self.pick_drive(p);
        }

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            Frame::new()
                .inner_margin(Margin::symmetric(12, 0))
                .show(ui, |ui| {
                    ui.label(small(if running {
                        "Other actions wait until this one finishes."
                    } else if util::is_root() {
                        "Running as root."
                    } else {
                        "Writing to a drive asks for your password first."
                    }));
                });
        });
    }

    fn footer(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        ui.set_min_height(40.0);
        ui.horizontal_centered(|ui| {
            if let Some(view) = &self.view {
                let running = view.ended.is_none();
                let hint = match &view.ended {
                    None => view.stop_warning.clone(),
                    Some(e) if e.ok => "Finished.".into(),
                    Some(_) => "The job failed. The output has the details.".into(),
                };
                ui.label(muted(hint).size(13.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if running {
                        let stop = Button::new(RichText::new("Stop…").color(DANGER_TEXT))
                            .stroke(Stroke::new(1.0, theme::tint(DANGER, 0.6)))
                            .min_size(Vec2::new(0.0, 40.0));
                        if ui.add(stop).clicked() {
                            self.stop_confirm = true;
                        }
                    } else if ui.add(theme::primary_button("Done")).clicked() {
                        self.view = None;
                        self.log.clear();
                    }
                });
                return;
            }
            let f = match self.page {
                Page::Backup => self.backup.footer(&self.disks),
                Page::Write => self.write.footer(&self.disks),
                Page::Clone => self.clone.footer(&self.disks),
            };
            let hint = match &f.action {
                Ok(_) => f.hint.clone(),
                Err(reason) => reason.clone(),
            };
            ui.label(muted(hint).size(13.0));
            let mut start = false;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let b = if f.danger {
                    theme::danger_button(&f.label)
                } else {
                    theme::primary_button(&f.label)
                };
                start = ui.add_enabled(f.action.is_ok(), b).clicked();
            });
            if start {
                if let Ok(a) = f.action {
                    self.start(a, ctx);
                }
            }
        });
    }

    fn progress(&mut self, ui: &mut Ui) {
        let Some(view) = &self.view else {
            return;
        };
        let elapsed = view
            .ended
            .as_ref()
            .map(|e| e.after)
            .unwrap_or_else(|| view.started.elapsed());
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(&view.title).size(26.0));
                ui.label(muted(&view.subtitle));
            });
            ui.with_layout(Layout::right_to_left(Align::TOP), |ui| match &view.ended {
                None => chip(ui, &format!("Running · {}", duration(elapsed)), ACCENT),
                Some(e) if e.ok => chip(
                    ui,
                    &format!("{} Finished in {}", theme::ICON_CHECK, duration(elapsed)),
                    OK,
                ),
                Some(_) => chip(
                    ui,
                    &format!("{} Failed after {}", theme::ICON_CROSS, duration(elapsed)),
                    DANGER_TEXT,
                ),
            });
        });
        ui.add_space(6.0);

        let steps = self.log.steps();
        match &view.ended {
            None => {
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let pct = self.log.progress();
                    ui.horizontal(|ui| {
                        let current = if self.log.pending_prompt().is_some() {
                            "Waiting for you to confirm"
                        } else {
                            steps.last().map(String::as_str).unwrap_or("Starting…")
                        };
                        ui.label(RichText::new(current).size(16.0));
                        if let Some(p) = pct {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    mono(format!("{:.0}%", p * 100.0)).size(28.0).color(ACCENT),
                                );
                            });
                        }
                    });
                    theme::progress_bar(ui, pct.unwrap_or(0.0));
                    if let Some(live) = self.log.live() {
                        ui.label(mono(live).color(MUTED));
                    }
                });
            }
            Some(e) if e.ok => theme::callout(ui, OK, theme::ICON_CHECK, "Done", |ui| {
                if let Some(last) = steps.last() {
                    ui.label(last.as_str());
                }
            }),
            Some(e) => theme::callout(
                ui,
                DANGER_TEXT,
                theme::ICON_CROSS,
                "It did not finish",
                |ui| {
                    ui.label(
                        e.message
                            .as_deref()
                            .unwrap_or("The job stopped with an error."),
                    );
                },
            ),
        }

        if !steps.is_empty() {
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 10.0;
                let last = steps.len() - 1;
                for (i, s) in steps.iter().enumerate() {
                    let (icon, color) = match (&view.ended, i == last) {
                        (None, true) => (theme::ICON_CURRENT, ACCENT),
                        (Some(e), true) if !e.ok => (theme::ICON_CROSS, DANGER_TEXT),
                        _ => (theme::ICON_CHECK, OK),
                    };
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [18.0, 18.0],
                            egui::Label::new(RichText::new(icon).color(color)),
                        );
                        let text = RichText::new(s.as_str());
                        ui.label(if color == ACCENT {
                            text.color(ACCENT)
                        } else {
                            text
                        });
                    });
                }
            });
        }

        CollapsingHeader::new(muted("Output"))
            .id_salt("output")
            .default_open(true)
            .show(ui, |ui| {
                if ui.add(theme::quiet_button("Copy")).clicked() {
                    ui.ctx().copy_text(self.log.text());
                }
                Frame::new()
                    .fill(theme::BG)
                    .stroke(Stroke::new(1.0, theme::BORDER))
                    .corner_radius(theme::LARGE_R)
                    .inner_margin(12)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let row_h = ui.text_style_height(&TextStyle::Monospace);
                        ScrollArea::both()
                            .id_salt("output-scroll")
                            .max_height(240.0)
                            .stick_to_bottom(true)
                            .auto_shrink([false, true])
                            .show_rows(ui, row_h, self.log.len(), |ui, rows| {
                                for i in rows {
                                    ui.add(
                                        egui::Label::new(
                                            mono(self.log.line(i)).color(MUTED).size(12.0),
                                        )
                                        .extend(),
                                    );
                                }
                            });
                    });
            });
    }

    /// The CLI's "retype the target path" prompt, as a dialog.
    fn confirm_modal(&mut self, ctx: &egui::Context) {
        let Some(path) = self.log.pending_prompt() else {
            return;
        };
        let Some(job) = self.job.as_mut() else {
            return;
        };
        let title = self
            .view
            .as_ref()
            .map(|v| v.title.clone())
            .unwrap_or_default();
        let drive = find(&self.disks, &path);
        let mut answer = None;
        let modal = egui::Modal::new(Id::new("confirm-erase")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            ui.label(
                RichText::new(format!("{} Erase {}?", theme::ICON_WARN, path))
                    .size(20.0)
                    .color(DANGER_TEXT),
            );
            ui.label(muted(match drive {
                Some(d) => format!(
                    "{}. {} ({}) is wiped first.",
                    title,
                    describe(d),
                    human(size(d))
                ),
                None => format!("{}. The drive is wiped first.", title),
            }));
            Frame::new()
                .fill(theme::tint(DANGER, 0.10))
                .stroke(Stroke::new(1.0, theme::tint(DANGER, 0.45)))
                .corner_radius(theme::LARGE_R)
                .inner_margin(Margin::symmetric(14, 12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 4.0;
                    ui.label(
                        RichText::new("Will be destroyed")
                            .color(DANGER_TEXT)
                            .size(13.0),
                    );
                    match drive {
                        Some(d) if disks::partitions(d).next().is_some() => {
                            for p in disks::partitions(d) {
                                ui.label(mono(format!(
                                    "{}   {}   {}",
                                    p.dev_path(),
                                    disks::fs_label(p),
                                    human(size(p))
                                )));
                            }
                        }
                        Some(d) => {
                            ui.label(mono(disks::holds_now(d)));
                        }
                        None => {
                            ui.label(mono(format!("Everything on {}", path)));
                        }
                    }
                });
            ui.label(muted(format!("Type {} to confirm", path)).size(13.0));
            let resp = ui.add(
                TextEdit::singleline(&mut self.prompt_input)
                    .font(TextStyle::Monospace)
                    .hint_text(path.as_str())
                    .desired_width(f32::INFINITY),
            );
            if self.prompt_input.is_empty() && !resp.has_focus() {
                resp.request_focus();
            }
            let matches = self.prompt_input.trim() == path;
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let erase = theme::danger_button(&format!("Erase {}", path));
                if ui.add_enabled(matches, erase).clicked() || (matches && enter) {
                    answer = Some(path.clone());
                }
                if ui
                    .add(Button::new("Cancel").min_size(Vec2::new(0.0, 40.0)))
                    .clicked()
                {
                    // A wrong answer makes the CLI abort before writing.
                    answer = Some(String::new());
                }
            });
        });
        if modal.should_close() && answer.is_none() {
            answer = Some(String::new());
        }
        if let Some(a) = answer {
            job.answer(&a, &mut self.log);
            self.prompt_input.clear();
        }
    }

    fn stop_modal(&mut self, ctx: &egui::Context) {
        if !self.stop_confirm {
            return;
        }
        let Some(job) = self.job.as_mut() else {
            self.stop_confirm = false;
            return;
        };
        let warning = self
            .view
            .as_ref()
            .map(|v| v.stop_warning.clone())
            .unwrap_or_default();
        let mut close = false;
        let modal = egui::Modal::new(Id::new("stop-job")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            ui.label(RichText::new("Stop this job?").size(20.0));
            ui.label(muted(warning));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(theme::danger_button("Stop")).clicked() {
                    job.stop(&mut self.log);
                    close = true;
                }
                if ui
                    .add(Button::new("Keep running").min_size(Vec2::new(0.0, 40.0)))
                    .clicked()
                {
                    close = true;
                }
            });
        });
        if close || modal.should_close() {
            self.stop_confirm = false;
        }
    }

    fn close_modal(&mut self, ctx: &egui::Context) {
        if !self.close_confirm {
            return;
        }
        let Some(job) = self.job.as_mut() else {
            self.close_confirm = false;
            return;
        };
        let mut keep = false;
        let modal = egui::Modal::new(Id::new("close-app")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            ui.label(RichText::new("A job is still running").size(20.0));
            ui.label(muted(format!(
                "\"{}\" has not finished. Quitting stops it, which can leave the drive unusable.",
                job.title
            )));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(theme::danger_button("Stop and quit")).clicked() {
                    job.stop(&mut self.log);
                    self.allow_close = true;
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui
                    .add(Button::new("Keep running").min_size(Vec2::new(0.0, 40.0)))
                    .clicked()
                {
                    keep = true;
                }
            });
        });
        if keep || modal.should_close() {
            self.close_confirm = false;
        }
    }

    fn draw(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.poll_job(&ctx);
        self.handle_close(&ctx);
        self.handle_drop(&ctx);

        egui::Panel::left("sidebar")
            .exact_size(264.0)
            .resizable(false)
            .frame(
                Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(Margin::symmetric(14, 20)),
            )
            .show(ui, |ui| self.sidebar(ui));
        egui::Panel::bottom("footer")
            .resizable(false)
            .frame(
                Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(Margin::symmetric(36, 14)),
            )
            .show(ui, |ui| self.footer(ui, &ctx));
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::BG)
                    .inner_margin(Margin::symmetric(36, 28)),
            )
            .show(ui, |ui| {
                ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 14.0;
                    if self.view.is_some() {
                        self.progress(ui);
                    } else {
                        match self.page {
                            Page::Backup => self.backup.ui(ui, &self.disks),
                            Page::Write => self.write.ui(ui, &self.disks),
                            Page::Clone => self.clone.ui(ui, &self.disks),
                        }
                    }
                });
            });

        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let rect = ctx.content_rect();
            let painter =
                ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("drop")));
            painter.rect_filled(rect, 0.0, theme::tint(theme::BG, 0.85));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop a backup folder or an image file",
                egui::FontId::proportional(22.0),
                ACCENT,
            );
        }

        self.confirm_modal(&ctx);
        self.stop_modal(&ctx);
        self.close_modal(&ctx);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

/// "12 s", "4 min 12 s"
fn duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{} s", s)
    } else {
        format!("{} min {} s", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &egui::Context, app: &mut App) {
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| app.draw(ui));
        out.textures_delta.clear();
    }

    #[test]
    fn every_screen_renders() {
        let ctx = egui::Context::default();
        let mut app = App::new(&ctx);
        for page in [Page::Backup, Page::Write, Page::Clone] {
            app.page = page;
            frame(&ctx, &mut app);
            frame(&ctx, &mut app);
        }
    }

    #[test]
    fn progress_screen_renders() {
        let ctx = egui::Context::default();
        let mut app = App::new(&ctx);
        app.view = Some(JobView {
            title: "Writing laptop to /dev/sdb".into(),
            subtitle: "disk-cloner backup".into(),
            stop_warning: "Keep the window open.".into(),
            started: Instant::now(),
            ended: None,
        });
        frame(&ctx, &mut app);
        app.view.as_mut().unwrap().ended = Some(Ended {
            ok: false,
            after: Duration::from_secs(75),
            message: Some("partclone failed".into()),
        });
        frame(&ctx, &mut app);
    }

    #[test]
    fn durations() {
        assert_eq!(duration(Duration::from_secs(12)), "12 s");
        assert_eq!(duration(Duration::from_secs(252)), "4 min 12 s");
    }
}
