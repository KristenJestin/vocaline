use super::*;
use std::io::Write;
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
struct Log(Mutex<Vec<(Instant, Did)>>);

impl Log {
    fn push(&self, did: Did) {
        self.0.lock().unwrap().push((Instant::now(), did));
    }

    fn all(&self) -> Vec<Did> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, d)| d.clone())
            .collect()
    }

    /// When the fakes saw the first `did`.
    fn when(&self, did: &Did) -> Instant {
        let seen = self.0.lock().unwrap();
        seen.iter().find(|(_, d)| d == did).expect("it happened").0
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
    /// Where the reads of the phrase offered last are told.
    reads: Mutex<Option<mpsc::Sender<Instant>>>,
    /// A clipboard history reads every new selection this long after it is set, whatever
    /// the shortcut.
    watched: Mutex<Option<Duration>>,
    /// The source of the clipboard answers for none of its types.
    unsaved: AtomicBool,
    log: Arc<Log>,
}

impl FakeClipboard {
    /// An application reads the clipboard: it gets the text it holds, and the read is told.
    fn read(&self, asked: Instant) -> String {
        let text = self.read_quietly();
        if let Some(reads) = self.reads.lock().unwrap().as_ref() {
            let _ = reads.send(asked);
        }
        text
    }

    /// What the clipboard holds, looked at without reading it as an application would.
    fn read_quietly(&self) -> String {
        let contents = self.contents.lock().unwrap();
        contents
            .iter()
            .find(|(mime, _)| mime == "text/plain")
            .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default()
    }
}

impl Clipboard for FakeClipboard {
    fn save(&self) -> io::Result<Contents> {
        self.log.push(Did::Save);
        if self.unsaved.load(Ordering::SeqCst) {
            return Err(io::Error::other("the source did not answer"));
        }
        Ok(self.contents.lock().unwrap().clone())
    }

    fn set_text(&self, text: &str) -> io::Result<mpsc::Receiver<Instant>> {
        *self.contents.lock().unwrap() = vec![("text/plain".into(), text.as_bytes().to_vec())];
        let (reads, offer) = mpsc::channel();
        match *self.watched.lock().unwrap() {
            Some(after) if after.is_zero() => {
                let _ = reads.send(Instant::now());
            }
            Some(after) => {
                let reads = reads.clone();
                thread::spawn(move || {
                    thread::sleep(after);
                    let _ = reads.send(Instant::now());
                });
            }
            None => {}
        }
        *self.reads.lock().unwrap() = Some(reads);
        self.log.push(Did::Set(text.into()));
        Ok(offer)
    }

    fn restore(&self, contents: Contents) -> io::Result<()> {
        *self.reads.lock().unwrap() = None;
        *self.contents.lock().unwrap() = contents.clone();
        self.log.push(Did::Restore(contents));
        Ok(())
    }
}

/// The keyboard, and the application behind it: on the paste shortcut it reads the clipboard,
/// at once or later (as a busy application does), or never.
struct FakeKeyboard {
    clipboard: Arc<FakeClipboard>,
    reads_after: Mutex<Option<Duration>>,
    /// What the clipboard held when the shortcut was pressed.
    held: Mutex<Vec<String>>,
    /// What the application pasted, in order.
    pasted: Arc<Mutex<Vec<String>>>,
    log: Arc<Log>,
}

impl Keyboard for FakeKeyboard {
    fn press(&self, shortcut: Shortcut) -> io::Result<()> {
        self.log.push(Did::Press(shortcut));
        if shortcut == Shortcut::Enter {
            return Ok(());
        }
        let asked = Instant::now();
        self.held
            .lock()
            .unwrap()
            .push(self.clipboard.read_quietly());
        match *self.reads_after.lock().unwrap() {
            Some(after) if after.is_zero() => {
                let text = self.clipboard.read(asked);
                self.pasted.lock().unwrap().push(text);
            }
            Some(after) => {
                let clipboard = Arc::clone(&self.clipboard);
                let pasted = Arc::clone(&self.pasted);
                thread::spawn(move || {
                    thread::sleep(after);
                    let text = clipboard.read(Instant::now());
                    pasted.lock().unwrap().push(text);
                });
            }
            None => {}
        }
        Ok(())
    }
}

/// How long the test setup gives the application to read the phrase.
const UNREAD: Duration = Duration::from_millis(400);
/// A busy application: it reads the clipboard well after the settle of the test setup.
const BUSY: Duration = Duration::from_millis(150);
/// How long the test setup lets a clipboard history read the phrase before the shortcut.
const QUIET: Duration = Duration::from_millis(20);

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
    keyboard: Arc<FakeKeyboard>,
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
        reads: Mutex::new(None),
        watched: Mutex::new(None),
        unsaved: AtomicBool::new(false),
        log: Arc::clone(&log),
    });
    let keyboard = Arc::new(FakeKeyboard {
        clipboard: Arc::clone(&clipboard),
        reads_after: Mutex::new(Some(Duration::ZERO)),
        held: Mutex::new(Vec::new()),
        pasted: Arc::new(Mutex::new(Vec::new())),
        log: Arc::clone(&log),
    });
    let output = Output::spawn(
        Ports {
            desktop: desktop.clone(),
            clipboard: clipboard.clone(),
            keyboard: keyboard.clone(),
        },
        Timing {
            poll: Duration::from_millis(2),
            settle: Duration::from_millis(30),
            unread: UNREAD,
            quiet: QUIET,
        },
    );
    Setup {
        output,
        desktop,
        clipboard,
        keyboard,
        log,
    }
}

impl Setup {
    fn application_reads_after(&self, after: Option<Duration>) {
        *self.keyboard.reads_after.lock().unwrap() = after;
    }

    fn pasted(&self) -> Vec<String> {
        self.keyboard.pasted.lock().unwrap().clone()
    }

    fn wait_for_pastes(&self, count: usize) -> Vec<String> {
        let deadline = Instant::now() + WAIT;
        loop {
            let pasted = self.pasted();
            if pasted.len() >= count {
                return pasted;
            }
            assert!(Instant::now() < deadline, "only {pasted:?}");
            thread::sleep(Duration::from_millis(2));
        }
    }

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

#[test]
fn the_shortcut_for_enter_is_the_return_key() {
    assert_eq!(Wtype::args(Shortcut::Enter), ["-k", "Return"]);
}

#[test]
fn beginning_a_dictation_tells_the_class_of_the_window_that_has_the_focus() {
    let s = setup();
    assert_eq!(s.output.begin(), Some("org.gnome.TextEditor".into()));
    s.click(TERMINAL);
    assert_eq!(s.output.begin(), Some("foot".into()));
    *s.desktop.focused.lock().unwrap() = None;
    assert_eq!(s.output.begin(), None);
}

#[test]
fn enter_is_pressed_once_after_the_paste() {
    let s = setup();
    s.begin();
    s.output.phrase("ls -la".into());
    s.output.enter();
    assert_eq!(
        s.log.wait_for(5),
        [
            Did::Save,
            Did::Set("ls -la".into()),
            Did::Press(Shortcut::CtrlV),
            Did::Press(Shortcut::Enter),
            Did::Restore(screenshot()),
        ]
    );
}

#[test]
fn enter_waits_for_the_application_to_read_the_clipboard() {
    let s = setup();
    s.begin();
    s.output.phrase("ls -la".into());
    s.output.enter();
    s.log.wait_for(5);
    let gap = s.log.when(&Did::Press(Shortcut::Enter)) - s.log.when(&Did::Press(Shortcut::CtrlV));
    // The settle of the test setup: Enter sooner would run the line before it is pasted.
    assert!(gap >= Duration::from_millis(25), "{gap:?}");
}

#[test]
fn enter_waits_until_the_modifier_is_released() {
    let s = setup();
    s.begin();
    s.output.phrase("ls".into());
    s.log.wait_for(3);
    s.desktop.modifier.store(true, Ordering::SeqCst);
    s.output.enter();
    thread::sleep(Duration::from_millis(100));
    assert!(!s.log.all().contains(&Did::Press(Shortcut::Enter)));
    s.desktop.modifier.store(false, Ordering::SeqCst);
    s.log.wait_for(4);
    assert_eq!(s.log.all()[3], Did::Press(Shortcut::Enter));
}

#[test]
fn enter_goes_to_the_window_of_the_dictation() {
    let s = setup();
    s.begin();
    s.output.phrase("ls".into());
    s.log.wait_for(3);
    s.click(TERMINAL);
    s.output.enter();
    let did = s.log.wait_for(6);
    assert_eq!(
        &did[3..5],
        [Did::Focus(EDITOR.into()), Did::Press(Shortcut::Enter)]
    );
}

#[test]
fn enter_is_not_pressed_when_nothing_was_pasted() {
    let s = setup();
    s.begin();
    s.output.enter();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(s.log.all(), []);
}

#[test]
fn the_shortcut_is_pressed_once_the_clipboard_holds_the_phrase() {
    let s = setup();
    s.begin();
    s.output.phrase("Un.".into());
    s.output.phrase("Deux.".into());
    s.log.wait_for(6);
    assert_eq!(*s.keyboard.held.lock().unwrap(), ["Un.", " Deux."]);
}

#[test]
fn the_user_clipboard_is_not_restored_before_the_application_reads_the_phrase() {
    let s = setup();
    s.application_reads_after(Some(BUSY));
    s.begin();
    s.output.phrase("Bonjour.".into());
    assert_eq!(s.wait_for_pastes(1), ["Bonjour."]);
    s.log.wait_for(4);
    assert_eq!(*s.clipboard.contents.lock().unwrap(), screenshot());
}

#[test]
fn a_read_before_the_shortcut_does_not_count_as_the_paste() {
    let s = setup();
    *s.clipboard.watched.lock().unwrap() = Some(Duration::ZERO);
    s.application_reads_after(Some(BUSY));
    s.begin();
    s.output.phrase("Bonjour.".into());
    assert_eq!(s.wait_for_pastes(1), ["Bonjour."]);
}

#[test]
fn the_clipboard_is_put_back_when_the_application_never_reads_the_phrase() {
    let s = setup();
    s.application_reads_after(None);
    s.begin();
    s.output.phrase("Bonjour.".into());
    s.log.wait_for(4);
    let gap = s.log.when(&Did::Restore(screenshot())) - s.log.when(&Did::Press(Shortcut::CtrlV));
    assert!(gap >= UNREAD, "{gap:?}");
}

#[test]
fn the_next_phrase_waits_until_the_application_read_the_last_one() {
    let s = setup();
    s.application_reads_after(Some(BUSY));
    s.begin();
    s.output.phrase("Un.".into());
    s.output.phrase("Deux.".into());
    assert_eq!(s.wait_for_pastes(2), ["Un.", " Deux."]);
}

#[test]
fn enter_waits_until_the_application_read_the_phrase() {
    let s = setup();
    s.application_reads_after(Some(BUSY));
    s.begin();
    s.output.phrase("ls -la".into());
    s.output.enter();
    s.log.wait_for(4);
    s.wait_for_pastes(1);
    let gap = s.log.when(&Did::Press(Shortcut::Enter)) - s.log.when(&Did::Press(Shortcut::CtrlV));
    assert!(gap >= BUSY, "{gap:?}");
}

#[test]
fn a_clipboard_history_that_reads_the_phrase_late_is_let_through_before_the_shortcut() {
    let s = setup();
    *s.clipboard.watched.lock().unwrap() = Some(QUIET / 2);
    s.application_reads_after(Some(BUSY));
    s.begin();
    s.output.phrase("Bonjour.".into());
    assert_eq!(s.wait_for_pastes(1), ["Bonjour."]);
}

/// A clipboard type as its source serves it: a pipe, and the end the source writes to.
fn served(mime: &str) -> ((String, io::PipeReader), io::PipeWriter) {
    let (reader, writer) = io::pipe().unwrap();
    ((mime.into(), reader), writer)
}

const STRAGGLER: Duration = Duration::from_millis(50);

#[test]
fn a_clipboard_none_of_whose_types_answers_is_not_saved() {
    let (pipe, _held_open) = served("SAVE_TARGETS");
    let started = Instant::now();
    assert!(drain(vec![pipe], STRAGGLER, BUSY).is_err());
    assert!(started.elapsed() < WAIT, "{:?}", started.elapsed());
}

#[test]
fn the_types_that_answer_are_kept_when_one_never_does() {
    let (text, mut writer) = served("text/plain");
    let (compound, _held_open) = served("COMPOUND_TEXT");
    let (html, mut html_writer) = served("text/html");
    writer.write_all(b"copied").unwrap();
    drop(writer);
    html_writer.write_all(b"<b>copied</b>").unwrap();
    drop(html_writer);
    let started = Instant::now();
    let kept = drain(vec![text, compound, html], STRAGGLER, WAIT).unwrap();
    assert_eq!(
        kept,
        vec![
            ("text/plain".to_string(), b"copied".to_vec()),
            ("text/html".to_string(), b"<b>copied</b>".to_vec()),
        ]
    );
    // Past the others, the one that never answers costs the straggler, not the cap.
    assert!(started.elapsed() < WAIT / 2, "{:?}", started.elapsed());
}

#[test]
fn a_source_slow_for_every_type_is_waited_for() {
    let (text, mut writer) = served("text/plain");
    let (html, mut html_writer) = served("text/html");
    thread::spawn(move || {
        thread::sleep(STRAGGLER * 4);
        writer.write_all(b"copied").unwrap();
        drop(writer);
        html_writer.write_all(b"<b>copied</b>").unwrap();
    });
    assert_eq!(
        drain(vec![text, html], STRAGGLER, WAIT).unwrap(),
        vec![
            ("text/plain".to_string(), b"copied".to_vec()),
            ("text/html".to_string(), b"<b>copied</b>".to_vec()),
        ]
    );
}

#[test]
fn a_type_still_streaming_after_another_is_read_is_kept() {
    let (text, writer) = served("text/plain");
    let (image, mut image_writer) = served("image/png");
    drop(writer);
    thread::spawn(move || {
        for _ in 0..6 {
            image_writer.write_all(&[7; 1024]).unwrap();
            thread::sleep(STRAGGLER / 2);
        }
    });
    assert_eq!(
        drain(vec![text, image], STRAGGLER, WAIT).unwrap(),
        vec![
            ("text/plain".to_string(), Vec::new()),
            ("image/png".to_string(), vec![7; 6 * 1024]),
        ]
    );
}

#[test]
fn a_phrase_is_pasted_when_the_clipboard_cannot_be_saved() {
    let s = setup();
    s.clipboard.unsaved.store(true, Ordering::SeqCst);
    s.begin();
    s.output.phrase("Bonjour.".into());
    assert_eq!(s.wait_for_pastes(1), ["Bonjour."]);
}
