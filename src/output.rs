//! The output: each final phrase is pasted where the dictation started, through the clipboard
//! and a paste shortcut, and the clipboard is put back as it was.
//!
//! The window manager, the clipboard and the keyboard are ports, so the logic is tested with
//! fakes. Pasting runs on its own thread, fed in order: the daemon never waits for it.
//!
//! What is dictated and what the clipboard holds belong to the user: nothing here logs them.

use serde::Deserialize;
use std::io::{self, Read};
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
    fn set_text(&self, text: &str) -> io::Result<()>;
    fn restore(&self, contents: Contents) -> io::Result<()>;
}

pub trait Keyboard: Send + Sync {
    fn press(&self, shortcut: Shortcut) -> io::Result<()>;
}

/// Where the daemon hands what it heard.
pub trait Paste: Send {
    /// A dictation starts: its phrases go to the window focused now.
    fn begin(&self);
    fn phrase(&self, text: String);
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
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            poll: Duration::from_millis(20),
            settle: Duration::from_millis(400),
        }
    }
}

enum Job {
    Begin(Option<Window>),
    Phrase(String),
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
            pasted_at: None,
        };
        thread::spawn(move || worker.run(queue));
        Output { desktop, jobs }
    }
}

impl Paste for Output {
    fn begin(&self) {
        // Asked now, on the daemon's thread: the worker may still be pasting the last
        // dictation, and the focus it gives would be its own.
        let target = self.desktop.focused().unwrap_or_else(|e| {
            eprintln!("vocaline: the focused window could not be read: {e}");
            None
        });
        let _ = self.jobs.send(Job::Begin(target));
    }

    fn phrase(&self, text: String) {
        let _ = self.jobs.send(Job::Phrase(text));
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
    pasted_at: Option<Instant>,
}

impl Worker {
    fn run(mut self, queue: mpsc::Receiver<Job>) {
        loop {
            // The user's clipboard comes back once no phrase follows the last one in time.
            let job = if self.saved.is_some() {
                match queue.recv_timeout(self.timing.settle) {
                    Ok(job) => job,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        self.restore();
                        continue;
                    }
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
            }
        }
    }

    fn paste(&mut self, text: &str) -> io::Result<()> {
        // A source that cannot tell is taken as nothing held: waiting forever would lose more.
        while self.ports.desktop.modifier_held().unwrap_or(false) {
            thread::sleep(self.timing.poll);
        }
        if self.saved.is_none() {
            self.saved = Some(self.ports.clipboard.save()?);
        }
        let class = self.focus_target();
        if let Some(at) = self.pasted_at {
            thread::sleep(self.timing.settle.saturating_sub(at.elapsed()));
        }
        self.ports.clipboard.set_text(text)?;
        self.ports.keyboard.press(shortcut_for(&class))?;
        self.pasted_at = Some(Instant::now());
        Ok(())
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
        let mut contents = Vec::new();
        for mime in types {
            let (mut pipe, _) = paste::get_contents(
                ClipboardType::Regular,
                Seat::Unspecified,
                MimeType::Specific(&mime),
            )
            .map_err(io::Error::other)?;
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes)?;
            contents.push((mime, bytes));
        }
        Ok(contents)
    }

    fn set_text(&self, text: &str) -> io::Result<()> {
        use wl_clipboard_rs::copy::{MimeType, Options, Source};
        let mut options = Options::new();
        // A clipboard history that honours the hint keeps no copy of what was dictated.
        options.sensitive(true);
        options
            .copy(Source::Bytes(text.as_bytes().into()), MimeType::Text)
            .map_err(io::Error::other)
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

#[cfg(test)]
mod tests;
