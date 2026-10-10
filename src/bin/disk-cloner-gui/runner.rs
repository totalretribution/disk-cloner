//! Runs the `disk-cloner` CLI as a child process and collects its output.
//!
//! Anything that touches a disk goes through the CLI rather than library calls:
//! it needs root (the GUI does not), it streams partclone's progress, and it
//! asks for the target path on stdin before erasing. Elevated runs go through
//! `pkexec`, so only the CLI process ever runs as root.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Instant;

use disk_cloner::util;
use eframe::egui;

/// What the CLI prints (without a newline) before reading the target path.
const CONFIRM_PROMPT: &str = "Type the target device path to confirm erase [";

/// Oldest lines are dropped past this, so a long run cannot eat memory.
const MAX_LINES: usize = 20_000;

/// Locate the CLI: next to this executable first, then on PATH.
fn cli_path() -> Result<PathBuf, String> {
    if let Ok(exe) = std::env::current_exe() {
        let p = exe.with_file_name("disk-cloner");
        if p.is_file() {
            return Ok(p);
        }
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join("disk-cloner"))
        .find(|p| p.is_file())
        .ok_or_else(|| "cannot find the disk-cloner binary next to the GUI or on PATH".into())
}

enum Chunk {
    Out(Vec<u8>),
    Err(Vec<u8>),
}

/// A running (or just finished) CLI process.
pub struct Job {
    pub title: String,
    pub started: Instant,
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Chunk>,
    /// Ran through pkexec, so this process cannot signal it directly.
    elevated: bool,
    status: Option<ExitStatus>,
    readers_done: bool,
}

/// How a job ended.
pub enum Outcome {
    Success,
    Failed(String),
}

impl Job {
    /// Start `disk-cloner <args>`, through pkexec when `elevate` is set and the
    /// GUI is not already root. Output is appended to `log` by [`Job::poll`].
    pub fn spawn(
        title: &str,
        args: &[String],
        elevate: bool,
        log: &mut Log,
        ctx: &egui::Context,
    ) -> Result<Job, String> {
        let cli = cli_path()?;
        let elevated = elevate && !util::is_root();
        let mut cmd = if elevated {
            let mut c = Command::new("pkexec");
            c.arg(&cli);
            c
        } else {
            Command::new(&cli)
        };
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group, so Stop also reaches the partclone/zstd
            // pipelines the CLI spawns.
            .process_group(0);

        log.note(&format!(
            "$ {}{} {}",
            if elevated { "pkexec " } else { "" },
            cli.display(),
            args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
        ));
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("starting {}: {}", cli.display(), e))?;

        let (tx, rx) = mpsc::channel();
        pump(
            child.stdout.take().expect("piped"),
            tx.clone(),
            Chunk::Out,
            ctx,
        );
        pump(child.stderr.take().expect("piped"), tx, Chunk::Err, ctx);

        Ok(Job {
            title: title.to_string(),
            started: Instant::now(),
            stdin: child.stdin.take(),
            child,
            rx,
            elevated,
            status: None,
            readers_done: false,
        })
    }

    /// Move new output into `log`. Returns the outcome once the process has
    /// exited and all of its output has been read.
    pub fn poll(&mut self, log: &mut Log) -> Option<Outcome> {
        loop {
            match self.rx.try_recv() {
                Ok(Chunk::Out(b)) => log.push(false, &b),
                Ok(Chunk::Err(b)) => log.push(true, &b),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.readers_done = true;
                    break;
                }
            }
        }
        if self.status.is_none() {
            self.status = self.child.try_wait().ok().flatten();
        }
        let status = self.status?;
        if !self.readers_done {
            return None;
        }
        log.flush();
        Some(match status.code() {
            Some(0) => Outcome::Success,
            Some(126) | Some(127) if self.elevated => {
                Outcome::Failed("authentication was cancelled or failed".into())
            }
            _ => Outcome::Failed(status.to_string()),
        })
    }

    /// Answer the CLI's "retype the target path" prompt.
    pub fn answer(&mut self, text: &str, log: &mut Log) {
        log.answer_prompt(text);
        if let Some(stdin) = self.stdin.as_mut() {
            // A failed write means the process is gone; poll() reports that.
            let _ = stdin.write_all(format!("{}\n", text).as_bytes());
            let _ = stdin.flush();
        }
    }

    /// Send SIGTERM to the whole process group. An elevated job needs pkexec
    /// (another password prompt) since this process cannot signal root's.
    pub fn stop(&mut self, log: &mut Log) {
        let group = format!("-{}", self.child.id());
        let mut cmd = if self.elevated {
            let mut c = Command::new("pkexec");
            c.arg("kill");
            c
        } else {
            Command::new("kill")
        };
        cmd.args(["-TERM", "--", &group]);
        match cmd.status() {
            Ok(s) if s.success() => log.note("» stop requested"),
            Ok(s) => log.note(&format!("» could not stop the job: kill exited with {}", s)),
            Err(e) => log.note(&format!("» could not stop the job: {}", e)),
        }
    }
}

/// Forward everything read from `r` to the UI thread in chunks.
fn pump(
    mut r: impl Read + Send + 'static,
    tx: Sender<Chunk>,
    wrap: fn(Vec<u8>) -> Chunk,
    ctx: &egui::Context,
) {
    let ctx = ctx.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(wrap(buf[..n].to_vec())).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            }
        }
        ctx.request_repaint();
    });
}

/// Quote an argument for display only, the way a shell would need it.
fn quote(a: &str) -> Cow<'_, str> {
    if !a.is_empty()
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c))
    {
        Cow::Borrowed(a)
    } else {
        Cow::Owned(format!("'{}'", a.replace('\'', "'\\''")))
    }
}

#[derive(Default, PartialEq)]
enum Ansi {
    #[default]
    Text,
    Escape,
    Csi,
}

/// The unfinished last line of one output stream. A carriage return makes the
/// next character start the line over, so `\r` progress bars update in place.
#[derive(Default)]
struct Stream {
    partial: Vec<u8>,
    cr: bool,
    esc: Ansi,
    /// The confirmation prompt on this line has been answered already.
    answered: bool,
}

/// Terminal-like scrollback for the CLI's stdout and stderr.
#[derive(Default)]
pub struct Log {
    lines: VecDeque<String>,
    out: Stream,
    err: Stream,
    /// Milestone lines ("Restoring #2 ...", "[1/3] Imaging ..."), in order.
    steps: Vec<String>,
    /// The CLI's final `Error: ...` message, if it printed one.
    error: Option<String>,
}

/// Starts of the lines the CLI prints as it moves from one stage to the next.
const STEP_PREFIXES: &[&str] = &[
    "[",
    "Saving partition table",
    "Restoring",
    "Recreating swap",
    "Copying",
    "Writing ",
    "Checking sha256",
    "Uncompressed size",
    "Flushing",
    "sha256 of written",
    "Leaving #",
];

impl Log {
    fn push(&mut self, is_err: bool, bytes: &[u8]) {
        for &b in bytes {
            let s = if is_err { &mut self.err } else { &mut self.out };
            // Drop ANSI escape sequences (colours, cursor moves).
            match s.esc {
                Ansi::Escape => {
                    s.esc = if b == b'[' { Ansi::Csi } else { Ansi::Text };
                    continue;
                }
                Ansi::Csi => {
                    if (0x40..=0x7E).contains(&b) {
                        s.esc = Ansi::Text;
                    }
                    continue;
                }
                Ansi::Text => {}
            }
            match b {
                0x1B => s.esc = Ansi::Escape,
                b'\r' => s.cr = true,
                b'\n' => {
                    let line = String::from_utf8_lossy(&s.partial).into_owned();
                    s.partial.clear();
                    s.cr = false;
                    s.answered = false;
                    self.commit(line);
                }
                _ => {
                    if s.cr {
                        s.partial.clear();
                        s.cr = false;
                    }
                    s.partial.push(b);
                }
            }
        }
    }

    fn commit(&mut self, line: String) {
        let trimmed = line.trim();
        // The CLI spaces its stages with blank lines; they only waste rows here.
        if trimmed.is_empty() {
            return;
        }
        if STEP_PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
            self.steps.push(trimmed.to_string());
        }
        if let Some(e) = trimmed.strip_prefix("Error: ") {
            self.error = Some(e.to_string());
        }
        self.lines.push_back(line);
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
        }
    }

    /// Commit any unfinished lines, e.g. when the process has exited.
    fn flush(&mut self) {
        for is_err in [false, true] {
            let s = if is_err { &mut self.err } else { &mut self.out };
            if !s.partial.is_empty() {
                let line = String::from_utf8_lossy(&s.partial).into_owned();
                *s = Stream::default();
                self.commit(line);
            }
        }
    }

    /// Add a line of the GUI's own.
    pub fn note(&mut self, text: &str) {
        self.flush();
        self.commit(text.to_string());
    }

    pub fn clear(&mut self) {
        *self = Log::default();
    }

    pub fn len(&self) -> usize {
        self.lines.len() + self.partials().count()
    }

    pub fn line(&self, i: usize) -> Cow<'_, str> {
        match self.lines.get(i) {
            Some(l) => Cow::Borrowed(l),
            None => self
                .partials()
                .nth(i - self.lines.len())
                .map(String::from_utf8_lossy)
                .unwrap_or_default(),
        }
    }

    pub fn text(&self) -> String {
        (0..self.len())
            .map(|i| self.line(i))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn partials(&self) -> impl Iterator<Item = &[u8]> {
        [&self.out, &self.err]
            .into_iter()
            .map(|s| s.partial.as_slice())
            .filter(|p| !p.is_empty())
    }

    /// Percentage shown on a live progress line (partclone's `Completed:
    /// 42.1%`, or `write`'s `... 42%`), if one is on screen.
    pub fn progress(&self) -> Option<f32> {
        let text = self.live()?;
        let pct = text.rfind('%')?;
        let head = &text[..pct];
        let start = head
            .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
            .map_or(0, |i| i + 1);
        head[start..]
            .parse::<f32>()
            .ok()
            .map(|v| (v / 100.0).clamp(0.0, 1.0))
    }

    /// The progress line currently updating in place, or the last line if it
    /// is one (a tool that prints each update on its own line).
    pub fn live(&self) -> Option<String> {
        self.partials()
            .map(|p| String::from_utf8_lossy(p).trim().to_string())
            .chain(self.lines.back().map(|l| l.trim().to_string()))
            .find(|t| t.contains('%'))
    }

    pub fn steps(&self) -> &[String] {
        &self.steps
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The device path the CLI is waiting for the user to retype, if any.
    pub fn pending_prompt(&self) -> Option<String> {
        if self.out.answered {
            return None;
        }
        let text = String::from_utf8_lossy(&self.out.partial);
        let rest = &text[text.find(CONFIRM_PROMPT)? + CONFIRM_PROMPT.len()..];
        let path = rest.strip_suffix("]: ")?;
        Some(path.to_string())
    }

    fn answer_prompt(&mut self, text: &str) {
        self.out.answered = true;
        self.out.partial.extend_from_slice(text.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carriage_return_overwrites_line() {
        let mut log = Log::default();
        log.push(true, b"Completed: 10.0%\rCompleted: 42.5%");
        assert_eq!(log.len(), 1);
        assert_eq!(log.line(0), "Completed: 42.5%");
        assert_eq!(log.progress(), Some(0.425));
        log.push(true, b"\r\nDone\n");
        assert_eq!(log.line(0), "Completed: 42.5%");
        assert_eq!(log.line(1), "Done");
        assert_eq!(log.progress(), None);
    }

    #[test]
    fn strips_ansi() {
        let mut log = Log::default();
        log.push(false, b"\x1b[1;31mred\x1b[0m text\n");
        assert_eq!(log.line(0), "red text");
    }

    #[test]
    fn detects_and_answers_prompt() {
        let mut log = Log::default();
        log.push(
            false,
            b"plan\n\nType the target device path to confirm erase [/dev/sdb]: ",
        );
        assert_eq!(log.pending_prompt().as_deref(), Some("/dev/sdb"));
        log.answer_prompt("/dev/sdb");
        assert_eq!(log.pending_prompt(), None);
        log.push(false, b"\nRestoring\n");
        assert!(log.text().contains("[/dev/sdb]: /dev/sdb\nRestoring"));
    }

    #[test]
    fn collects_steps_and_error() {
        let mut log = Log::default();
        log.push(
            false,
            b"Restore image from : /x\n\nRestoring partition table to /dev/sdb\n",
        );
        log.push(false, b"\n[2/3] Imaging /dev/sda2 with partclone.ext4\n");
        log.push(true, b"Error: partclone.ext4 failed\n");
        assert_eq!(
            log.steps(),
            [
                "Restoring partition table to /dev/sdb",
                "[2/3] Imaging /dev/sda2 with partclone.ext4"
            ]
        );
        assert_eq!(log.error(), Some("partclone.ext4 failed"));
    }

    #[test]
    fn write_progress_line() {
        let mut log = Log::default();
        log.push(true, b"\r  1.2 GiB written, 80.0 MiB/s, 37%   ");
        assert_eq!(log.progress(), Some(0.37));
    }
}
