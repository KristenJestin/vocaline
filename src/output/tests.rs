use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const WAIT: Duration = Duration::from_secs(5);

/// What the fakes saw, in order.
#[derive(Debug, Clone, PartialEq)]
enum Did {
    Focus(String),
    Save,
    Set(String),
    Press(Shortcut),
    Restore(Contents),
}

#[derive(Default)]
struct Log(Mutex<Vec<Did>>);

impl Log {
    fn push(&self, did: Did) {
        self.0.lock().unwrap().push(did);
    }

    fn all(&self) -> Vec<Did> {
        self.0.lock().unwrap().clone()
    }

    fn wait_for(&self, count: usize) -> Vec<Did> {
        let deadline = Instant::now() + WAIT;
        loop {
            let all = self.all();
            if all.len() >= count {
                return all;
            }
            assert!(Instant::now() < deadline, "only {all:?}");
            thread::sleep(Duration::from_millis(2));
        }
    }
}

fn window(address: &str, class: &str) -> Window {
    Window {
        address: address.into(),
        class: class.into(),
    }
}

/// A window manager whose focus a test moves by hand, as a click would.
struct FakeDesktop {
    windows: Vec<Window>,
    focused: Mutex<Option<Window>>,
    modifier: AtomicBool,
    log: Arc<Log>,
}

impl Desktop for FakeDesktop {
    fn focused(&self) -> io::Result<Option<Window>> {
        Ok(self.focused.lock().unwrap().clone())
    }

    fn focus(&self, window: &Window) -> io::Result<()> {
        if !self.windows.contains(window) {
            return Err(io::Error::other("window not found"));
        }
        self.log.push(Did::Focus(window.address.clone()));
        *self.focused.lock().unwrap() = Some(window.clone());
        Ok(())
    }

    fn modifier_held(&self) -> io::Result<bool> {
        Ok(self.modifier.load(Ordering::SeqCst))
    }
}

struct FakeClipboard {
    contents: Mutex<Contents>,
    log: Arc<Log>,
}

impl Clipboard for FakeClipboard {
    fn save(&self) -> io::Result<Contents> {
        self.log.push(Did::Save);
        Ok(self.contents.lock().unwrap().clone())
    }

    fn set_text(&self, text: &str) -> io::Result<()> {
        *self.contents.lock().unwrap() = vec![("text/plain".into(), text.as_bytes().to_vec())];
        self.log.push(Did::Set(text.into()));
        Ok(())
    }

    fn restore(&self, contents: Contents) -> io::Result<()> {
        *self.contents.lock().unwrap() = contents.clone();
        self.log.push(Did::Restore(contents));
        Ok(())
    }
}

struct FakeKeyboard {
    log: Arc<Log>,
}

impl Keyboard for FakeKeyboard {
    fn press(&self, shortcut: Shortcut) -> io::Result<()> {
        self.log.push(Did::Press(shortcut));
        Ok(())
    }
}

const EDITOR: &str = "0xe";
const TERMINAL: &str = "0xt";

/// What the user copied before dictating: a screenshot, offered as two types.
fn screenshot() -> Contents {
    vec![
        ("image/png".into(), vec![0x89, b'P', b'N', b'G']),
        ("image/jpeg".into(), vec![0xff, 0xd8]),
    ]
}

struct Setup {
    output: Output,
    desktop: Arc<FakeDesktop>,
    clipboard: Arc<FakeClipboard>,
    log: Arc<Log>,
}

/// An editor with the focus, a terminal beside it, a screenshot in the clipboard.
fn setup() -> Setup {
    let log = Arc::new(Log::default());
    let editor = window(EDITOR, "org.gnome.TextEditor");
    let desktop = Arc::new(FakeDesktop {
        windows: vec![editor.clone(), window(TERMINAL, "foot")],
        focused: Mutex::new(Some(editor)),
        modifier: AtomicBool::new(false),
        log: Arc::clone(&log),
    });
    let clipboard = Arc::new(FakeClipboard {
        contents: Mutex::new(screenshot()),
        log: Arc::clone(&log),
    });
    let output = Output::spawn(
        Ports {
            desktop: desktop.clone(),
            clipboard: clipboard.clone(),
            keyboard: Arc::new(FakeKeyboard {
                log: Arc::clone(&log),
            }),
        },
        Timing {
            poll: Duration::from_millis(2),
            settle: Duration::from_millis(30),
        },
    );
    Setup {
        output,
        desktop,
        clipboard,
        log,
    }
}

impl Setup {
    fn click(&self, address: &str) {
        let window = self
            .desktop
            .windows
            .iter()
            .find(|w| w.address == address)
            .cloned();
        *self.desktop.focused.lock().unwrap() = window;
    }

    fn begin(&self) {
        self.output.begin();
    }
}

#[test]
fn phrases_land_in_the_window_focused_when_the_dictation_started() {
    let s = setup();
    s.begin();
    s.click(TERMINAL);
    s.output.phrase("Bonjour.".into());
    let did = s.log.wait_for(5);
    assert_eq!(
        did,
        [
            Did::Save,
            Did::Focus(EDITOR.into()),
            Did::Set("Bonjour.".into()),
            Did::Press(Shortcut::CtrlV),
            Did::Restore(screenshot()),
        ]
    );
}

#[test]
fn a_window_that_kept_the_focus_is_not_focused_again() {
    let s = setup();
    s.begin();
    s.output.phrase("Bonjour.".into());
    let did = s.log.wait_for(4);
    assert!(!did.iter().any(|d| matches!(d, Did::Focus(_))), "{did:?}");
}

#[test]
fn the_clipboard_is_put_back_with_every_type_it_held() {
    let s = setup();
    s.begin();
    s.output.phrase("Bonjour.".into());
    s.log.wait_for(4);
    assert_eq!(*s.clipboard.contents.lock().unwrap(), screenshot());
}

#[test]
fn phrases_that_follow_each_other_save_and_restore_the_clipboard_once() {
    let s = setup();
    s.begin();
    s.output.phrase("Un.".into());
    s.output.phrase("Deux.".into());
    let did = s.log.wait_for(6);
    assert_eq!(
        did,
        [
            Did::Save,
            Did::Set("Un.".into()),
            Did::Press(Shortcut::CtrlV),
            Did::Set(" Deux.".into()),
            Did::Press(Shortcut::CtrlV),
            Did::Restore(screenshot()),
        ]
    );
}

#[test]
fn the_paste_waits_until_the_modifier_is_released() {
    let s = setup();
    s.desktop.modifier.store(true, Ordering::SeqCst);
    s.begin();
    s.output.phrase("Bonjour.".into());
    thread::sleep(Duration::from_millis(100));
    assert!(
        !s.log.all().iter().any(|d| matches!(d, Did::Press(_))),
        "{:?}",
        s.log.all()
    );
    s.desktop.modifier.store(false, Ordering::SeqCst);
    let did = s.log.wait_for(4);
    assert_eq!(did[2], Did::Press(Shortcut::CtrlV));
}

#[test]
fn phrases_are_separated_by_a_space_and_the_first_of_a_dictation_has_none() {
    let s = setup();
    s.begin();
    s.output.phrase("Bonjour, ça va ?".into());
    s.output.phrase("Très bien.".into());
    s.log.wait_for(6);
    s.begin();
    s.output.phrase("Encore une.".into());
    let typed: Vec<String> = s
        .log
        .wait_for(10)
        .into_iter()
        .filter_map(|d| match d {
            Did::Set(text) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(typed, ["Bonjour, ça va ?", " Très bien.", "Encore une."]);
}

#[test]
fn the_engine_spaces_around_a_phrase_are_not_pasted() {
    let s = setup();
    s.begin();
    s.output.phrase(" Bonjour. ".into());
    s.output.phrase("   ".into());
    s.output.phrase(" Fin.".into());
    let did = s.log.wait_for(6);
    assert_eq!(did[1], Did::Set("Bonjour.".into()));
    assert_eq!(did[3], Did::Set(" Fin.".into()));
}

#[test]
fn a_terminal_gets_ctrl_shift_v() {
    let s = setup();
    s.click(TERMINAL);
    s.begin();
    s.output.phrase("ls".into());
    let did = s.log.wait_for(4);
    assert_eq!(did[2], Did::Press(Shortcut::CtrlShiftV));
}

#[test]
fn a_target_window_that_closed_gets_the_phrase_where_the_focus_is() {
    let s = setup();
    *s.desktop.focused.lock().unwrap() = Some(window("0xgone", "org.gnome.TextEditor"));
    s.begin();
    s.click(TERMINAL);
    s.output.phrase("Bonjour.".into());
    let did = s.log.wait_for(4);
    assert_eq!(
        &did[1..3],
        [
            Did::Set("Bonjour.".into()),
            Did::Press(Shortcut::CtrlShiftV)
        ]
    );
}

#[test]
fn terminals_are_known_by_their_class() {
    for class in ["foot", "Alacritty", "kitty", "com.mitchellh.ghostty"] {
        assert_eq!(shortcut_for(class), Shortcut::CtrlShiftV, "{class}");
    }
    for class in ["org.gnome.TextEditor", "zen", "code", ""] {
        assert_eq!(shortcut_for(class), Shortcut::CtrlV, "{class}");
    }
}

#[test]
fn the_active_window_is_read_from_hyprctl() {
    let json = r#"{"address":"0x5633","class":"foot","title":"~","pid":1}"#;
    assert_eq!(
        Hyprland::parse_active(json.as_bytes()).unwrap(),
        Some(window("0x5633", "foot"))
    );
    assert_eq!(Hyprland::parse_active(b"{}").unwrap(), None);
}

#[test]
fn a_window_is_focused_by_its_address() {
    assert_eq!(
        Hyprland::focus_args(&window("0x5633", "foot")),
        ["dispatch", r#"hl.dsp.focus({ window = "address:0x5633" })"#]
    );
}

#[test]
fn the_shortcut_is_typed_by_wtype() {
    assert_eq!(
        Wtype::args(Shortcut::CtrlV),
        ["-M", "ctrl", "-k", "v", "-m", "ctrl"]
    );
    assert_eq!(
        Wtype::args(Shortcut::CtrlShiftV),
        [
            "-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl"
        ]
    );
}
