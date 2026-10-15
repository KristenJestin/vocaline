//! The output: each final phrase is pasted where the dictation started, through the clipboard
//! and a paste shortcut, and the clipboard is put back as it was.
//!
//! The window manager, the clipboard and the keyboard are ports, so the logic is tested with
//! fakes. Pasting runs on its own thread, fed in order: the daemon never waits for it.
//!
//! What is dictated and what the clipboard holds belong to the user: nothing here logs them.

use serde::Deserialize;
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// The keysyms Hyprland is asked about: while one is held, Ctrl+V would become, say,
/// SUPER+Ctrl+V and paste nothing (or run a binding).
const MODIFIERS: [&str; 9] = [
    "Super_L",
    "Super_R",
    "Control_L",
    "Control_R",
    "Alt_L",
    "Alt_R",
    "Shift_L",
    "Shift_R",
    "ISO_Level3_Shift",
];

/// Window classes that paste with Ctrl+Shift+V, where Ctrl+V is a control character.
const TERMINALS: [&str; 12] = [
    "foot",
    "footclient",
    "alacritty",
    "kitty",
    "com.mitchellh.ghostty",
    "org.wezfurlong.wezterm",
    "org.gnome.console",
    "org.gnome.ptyxis",
    "org.kde.konsole",
    "xterm",
    "urxvt",
    "st",
];

/// The clipboard as it was: each MIME type it offered, with its bytes.
pub type Contents = Vec<(String, Vec<u8>)>;

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub address: String,
    pub class: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    CtrlV,
    CtrlShiftV,
    Enter,
}

/// The window manager.
pub trait Desktop: Send + Sync {
    fn focused(&self) -> io::Result<Option<Window>>;
    fn focus(&self, window: &Window) -> io::Result<()>;
    /// Whether a modifier key is held down on a real keyboard.
    fn modifier_held(&self) -> io::Result<bool>;
}

pub trait Clipboard: Send + Sync {
    fn save(&self) -> io::Result<Contents>;
    /// Offers the text, and returns once the compositor holds it: the focused application then
    /// sees it before any key typed afterwards. Each read of it is told, once served, by when
    /// it was asked.
    fn set_text(&self, text: &str) -> io::Result<mpsc::Receiver<Instant>>;
    fn restore(&self, contents: Contents) -> io::Result<()>;
}

pub trait Keyboard: Send + Sync {
    fn press(&self, shortcut: Shortcut) -> io::Result<()>;
}

/// Where the daemon hands what it heard.
pub trait Paste: Send {
    /// A dictation starts: its phrases go to the window focused now, whose class is returned.
    fn begin(&self) -> Option<String>;
    fn phrase(&self, text: String);
    /// Presses Enter in the window of the dictation, once what was handed over is pasted.
    fn enter(&self);
}

pub struct Ports {
    pub desktop: Arc<dyn Desktop>,
    pub clipboard: Arc<dyn Clipboard>,
    pub keyboard: Arc<dyn Keyboard>,
}

pub struct Timing {
    /// How often a held modifier is checked again.
    pub poll: Duration,
    /// How long the application is given to read the clipboard after the shortcut: it reads
    /// it afterwards, on its own time, so changing the clipboard sooner pastes the wrong text.
    pub settle: Duration,
    /// How long a busy application is waited for when it has not read the phrase within the
    /// settle: past it, the shortcut is taken as lost and the clipboard comes back.
    pub unread: Duration,
    /// How long no read of a new phrase is waited for before the shortcut: a clipboard history
    /// reads each new selection at once, and its reads must not be taken for the application's.
    pub quiet: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            poll: Duration::from_millis(20),
            settle: Duration::from_millis(400),
            unread: Duration::from_secs(2),
            quiet: Duration::from_millis(15),
        }
    }
}

enum Job {
    Begin(Option<Window>),
    Phrase(String),
    Enter,
}

pub struct Output {
    desktop: Arc<dyn Desktop>,
    jobs: mpsc::Sender<Job>,
}

impl Output {
    pub fn spawn(ports: Ports, timing: Timing) -> Output {
        let (jobs, queue) = mpsc::channel();
        let desktop = Arc::clone(&ports.desktop);
        let worker = Worker {
            ports,
            timing,
            target: None,
            first: true,
            saved: None,
            last: None,
        };
        thread::spawn(move || worker.run(queue));
        Output { desktop, jobs }
    }
}

impl Paste for Output {
    fn begin(&self) -> Option<String> {
        // Asked now, on the daemon's thread: the worker may still be pasting the last
        // dictation, and the focus it gives would be its own.
        let target = self.desktop.focused().unwrap_or_else(|e| {
            eprintln!("vocaline: the focused window could not be read: {e}");
            None
        });
        let class = target.as_ref().map(|window| window.class.clone());
        let _ = self.jobs.send(Job::Begin(target));
        class
    }

    fn phrase(&self, text: String) {
        let _ = self.jobs.send(Job::Phrase(text));
    }

    fn enter(&self) {
        let _ = self.jobs.send(Job::Enter);
    }
}

struct Worker {
    ports: Ports,
    timing: Timing,
    /// The window focused when the dictation started.
    target: Option<Window>,
    /// No phrase of this dictation was pasted yet: no space before it.
    first: bool,
    /// The user's clipboard, while ours holds a phrase.
    saved: Option<Contents>,
    last: Option<Pasted>,
}

/// The last phrase pasted, which the application may not have read yet.
struct Pasted {
    /// When the shortcut was pressed: a read asked sooner is a clipboard history's, not the
    /// application's.
    at: Instant,
    reads: mpsc::Receiver<Instant>,
    read: bool,
}

impl Pasted {
    fn read(&mut self) -> bool {
        if !self.read {
            let at = self.at;
            self.read = self.reads.try_iter().any(|asked| asked >= at);
        }
        self.read
    }
}

impl Worker {
    fn run(mut self, queue: mpsc::Receiver<Job>) {
        loop {
            // The user's clipboard comes back once the application has read the last phrase and
            // no phrase follows it in time.
            let job = if self.saved.is_some() {
                let until = self.until_restore();
                if until.is_zero() {
                    self.restore();
                    continue;
                }
                // The reads do not come on this queue: they are looked at again every poll.
                match queue.recv_timeout(until.min(self.timing.poll)) {
                    Ok(job) => job,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return self.restore(),
                }
            } else {
                match queue.recv() {
                    Ok(job) => job,
                    Err(_) => return,
                }
            };
            match job {
                Job::Begin(target) => {
                    self.target = target;
                    self.first = true;
                }
                Job::Phrase(text) => {
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let text = if self.first {
                        text.to_string()
                    } else {
                        format!(" {text}")
                    };
                    match self.paste(&text) {
                        Ok(()) => self.first = false,
                        Err(e) => eprintln!("vocaline: a phrase could not be pasted: {e}"),
                    }
                }
                // Nothing pasted, nothing to send: Enter alone would run an empty line or send
                // an empty message.
                Job::Enter if self.first => {}
                Job::Enter => {
                    if let Err(e) = self.enter() {
                        eprintln!("vocaline: Enter could not be pressed: {e}");
                    }
                }
            }
        }
    }

    fn wait_for_modifiers(&self) {
        // A source that cannot tell is taken as nothing held: waiting forever would lose more.
        while self.ports.desktop.modifier_held().unwrap_or(false) {
            thread::sleep(self.timing.poll);
        }
    }

    fn paste(&mut self, text: &str) -> io::Result<()> {
        self.wait_for_modifiers();
        if self.saved.is_none() {
            self.saved = Some(self.ports.clipboard.save()?);
        }
        let class = self.focus_target();
        self.wait_for_read();
        let reads = self.ports.clipboard.set_text(text)?;
        while reads.recv_timeout(self.timing.quiet).is_ok() {}
        let at = Instant::now();
        self.ports.keyboard.press(shortcut_for(&class))?;
        self.last = Some(Pasted {
            at,
            reads,
            read: false,
        });
        Ok(())
    }

    fn enter(&mut self) -> io::Result<()> {
        self.wait_for_modifiers();
        self.focus_target();
        // Enter sooner than the application's read of the clipboard would run the line before
        // the text is in it.
        self.wait_for_read();
        self.ports.keyboard.press(Shortcut::Enter)
    }

    /// Waits until the application has read the last phrase, and the settle after its
    /// shortcut: a busy one reads it late, so changing the clipboard after a fixed time pastes
    /// the wrong text. One that has not read it within `unread` never will.
    fn wait_for_read(&mut self) {
        let Some(last) = &mut self.last else {
            return;
        };
        while !last.read() && last.at.elapsed() < self.timing.unread {
            thread::sleep(self.timing.poll);
        }
        thread::sleep(self.timing.settle.saturating_sub(last.at.elapsed()));
    }

    /// How long until the user's clipboard may come back: the settle after the shortcut once
    /// the application has read the phrase, `unread` after it while it has not.
    fn until_restore(&mut self) -> Duration {
        let Some(last) = &mut self.last else {
            return Duration::ZERO;
        };
        let after = if last.read() {
            self.timing.settle
        } else {
            self.timing.unread
        };
        after.saturating_sub(last.at.elapsed())
    }

    /// Gives the focus back to the window of the dictation, and returns the class of the
    /// window that will receive the paste.
    fn focus_target(&mut self) -> String {
        let focused = self.ports.desktop.focused().ok().flatten();
        if let Some(target) = &self.target
            && focused.as_ref().map(|w| &w.address) != Some(&target.address)
        {
            match self.ports.desktop.focus(target) {
                Ok(()) => return target.class.clone(),
                Err(e) => {
                    // Closed since, most likely: the phrase is not lost, it goes where the
                    // user is now.
                    eprintln!("vocaline: the window of the dictation could not be focused: {e}");
                    self.target = None;
                }
            }
        }
        focused.map(|w| w.class).unwrap_or_default()
    }

    fn restore(&mut self) {
        if let Some(contents) = self.saved.take()
            && let Err(e) = self.ports.clipboard.restore(contents)
        {
            eprintln!("vocaline: the clipboard could not be put back: {e}");
        }
    }
}

pub fn shortcut_for(class: &str) -> Shortcut {
    if TERMINALS.iter().any(|t| t.eq_ignore_ascii_case(class)) {
        Shortcut::CtrlShiftV
    } else {
        Shortcut::CtrlV
    }
}

/// Hyprland, through `hyprctl`.
pub struct Hyprland;

impl Hyprland {
    fn parse_active(json: &[u8]) -> io::Result<Option<Window>> {
        #[derive(Deserialize)]
        struct Active {
            address: Option<String>,
            #[serde(default)]
            class: String,
        }
        let active: Active = serde_json::from_slice(json).map_err(io::Error::other)?;
        Ok(active.address.map(|address| Window {
            address,
            class: active.class,
        }))
    }

    fn focus_args(window: &Window) -> [String; 2] {
        [
            "dispatch".into(),
            format!(
                r#"hl.dsp.focus({{ window = "address:{}" }})"#,
                window.address
            ),
        ]
    }
}

/// Runs `hyprctl` and returns what it printed.
fn hyprctl(args: &[String]) -> io::Result<Vec<u8>> {
    let output = Command::new("hyprctl").args(args).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "hyprctl {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stdout).trim()
        )));
    }
    Ok(output.stdout)
}

impl Desktop for Hyprland {
    fn focused(&self) -> io::Result<Option<Window>> {
        Hyprland::parse_active(&hyprctl(&["-j".into(), "activewindow".into()])?)
    }

    fn focus(&self, window: &Window) -> io::Result<()> {
        // A window that is gone is a warning, not a failure, to hyprctl.
        let answer = hyprctl(&Hyprland::focus_args(window))?;
        match String::from_utf8_lossy(&answer).trim() {
            "ok" => Ok(()),
            other => Err(io::Error::other(other.to_string())),
        }
    }

    fn modifier_held(&self) -> io::Result<bool> {
        // The keys Hyprland itself sees pressed, so a binding's modifier counts and the
        // virtual keyboard that types the shortcut does not.
        let held: Vec<String> = MODIFIERS
            .iter()
            .map(|key| format!("hl.is_key_down(\"{key}\")"))
            .collect();
        let answer = hyprctl(&["repl".into(), format!("return {}", held.join(" or "))])?;
        match String::from_utf8_lossy(&answer).trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            other => Err(io::Error::other(other.to_string())),
        }
    }
}

/// Types the shortcut on a virtual keyboard.
pub struct Wtype;

impl Wtype {
    fn args(shortcut: Shortcut) -> Vec<&'static str> {
        match shortcut {
            Shortcut::CtrlV => vec!["-M", "ctrl", "-k", "v", "-m", "ctrl"],
            Shortcut::CtrlShiftV => vec![
                "-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl",
            ],
            Shortcut::Enter => vec!["-k", "Return"],
        }
    }
}

impl Keyboard for Wtype {
    fn press(&self, shortcut: Shortcut) -> io::Result<()> {
        let status = Command::new("wtype").args(Wtype::args(shortcut)).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("wtype failed: {status}")))
        }
    }
}

/// The Wayland clipboard, through the data-control protocol: unlike `wl-copy`, it can offer
/// every MIME type the clipboard held at once.
pub struct Wayland;

impl Clipboard for Wayland {
    fn save(&self) -> io::Result<Contents> {
        use wl_clipboard_rs::paste::{self, ClipboardType, Error, MimeType, Seat};
        let types = match paste::get_mime_types_ordered(ClipboardType::Regular, Seat::Unspecified) {
            Ok(types) => types,
            Err(Error::ClipboardEmpty | Error::NoSeats) => return Ok(Vec::new()),
            Err(e) => return Err(io::Error::other(e)),
        };
        let mut pipes = Vec::new();
        for mime in types {
            let (pipe, _) = paste::get_contents(
                ClipboardType::Regular,
                Seat::Unspecified,
                MimeType::Specific(&mime),
            )
            .map_err(io::Error::other)?;
            pipes.push((mime, pipe));
        }
        Ok(drain(pipes, STRAGGLER, UNSERVED))
    }

    fn set_text(&self, text: &str) -> io::Result<mpsc::Receiver<Instant>> {
        selection::offer(text)
    }

    fn restore(&self, contents: Contents) -> io::Result<()> {
        use wl_clipboard_rs::copy::{self, ClipboardType, MimeSource, MimeType, Options, Seat};
        if contents.is_empty() {
            return copy::clear(ClipboardType::Regular, Seat::All).map_err(io::Error::other);
        }
        let sources = contents
            .into_iter()
            .map(|(mime, bytes)| MimeSource {
                source: copy::Source::Bytes(bytes.into()),
                mime_type: MimeType::Specific(mime),
            })
            .collect();
        let mut options = Options::new();
        options.omit_additional_text_mime_types(true);
        options.copy_multi(sources).map_err(io::Error::other)
    }
}

/// How long a type is waited for once every other one is read: a source may offer a type it
/// never serves (a Firefox-based browser offers `COMPOUND_TEXT` and `SAVE_TARGETS`), and
/// keeps its pipe open without writing.
const STRAGGLER: Duration = Duration::from_millis(100);
/// How long a source is waited for at most: a busy one is slow for every type, not stuck.
const UNSERVED: Duration = Duration::from_secs(2);

/// Reads every pipe to its end, all at once. A type not read in time is dropped: the rest of
/// the clipboard comes back without it, rather than no paste ever again.
fn drain<P: Read + AsFd>(pipes: Vec<(String, P)>, straggler: Duration, cap: Duration) -> Contents {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::fs::{OFlags, fcntl_setfl};
    // Each type with what was read of it, and whether its pipe is still open.
    let mut types: Vec<_> = pipes
        .into_iter()
        .map(|(mime, pipe)| {
            let open = fcntl_setfl(&pipe, OFlags::NONBLOCK).is_ok();
            (mime, pipe, Vec::new(), open)
        })
        .collect();
    let mut read = vec![false; types.len()];
    let started = Instant::now();
    let mut last_read = None;
    loop {
        let deadline = match last_read {
            Some(at) => (started + cap).min(at + straggler),
            None => started + cap,
        };
        let open: Vec<usize> = (0..types.len()).filter(|&i| types[i].3).collect();
        let left = deadline.saturating_duration_since(Instant::now());
        if open.is_empty() || left.is_zero() {
            break;
        }
        let mut fds: Vec<_> = open
            .iter()
            .map(|&i| PollFd::new(&types[i].1, PollFlags::IN))
            .collect();
        let timeout = Timespec::try_from(left).ok();
        if poll(&mut fds, timeout.as_ref()).is_err_and(|e| e != rustix::io::Errno::INTR) {
            break;
        }
        let ready: Vec<usize> = open
            .into_iter()
            .zip(&fds)
            .filter(|(_, fd)| !fd.revents().is_empty())
            .map(|(i, _)| i)
            .collect();
        drop(fds);
        for i in ready {
            let (_, pipe, bytes, open) = &mut types[i];
            let mut chunk = [0; 64 * 1024];
            match pipe.read(&mut chunk) {
                Ok(0) => {
                    *open = false;
                    read[i] = true;
                    last_read = Some(Instant::now());
                }
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // A source that failed is one type less, as one that never answers.
                Err(_) => {
                    *open = false;
                    last_read = Some(Instant::now());
                }
            }
        }
    }
    types
        .into_iter()
        .zip(read)
        .filter(|(_, read)| *read)
        .map(|((mime, _, bytes, _), _)| (mime, bytes))
        .collect()
}

mod selection;

#[cfg(test)]
mod tests;
