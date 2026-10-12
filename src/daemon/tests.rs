use super::*;
use crate::audio::Capture;
use crate::engine::{EngineInput, Sink};
use crate::rules::Rule;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

// How long a test waits for something that must arrive. Generous, because a loaded machine
// (a build running beside the tests) can stall a thread for seconds; it costs nothing when the
// thing arrives at once, only a failing test waits all of it.
const WAIT: Duration = Duration::from_secs(30);

/// A microphone that hands over 100 ms of a steady tone every 5 ms, `chunks` times (forever
/// when `None`), and counts the captures left open.
#[derive(Default)]
struct FakeMic {
    chunks: Option<usize>,
    fail: bool,
    /// The capture opens, then fails on its first read, as `pw-record` does without PipeWire.
    read_error: Option<&'static str>,
    opened: AtomicUsize,
    open_now: Arc<AtomicUsize>,
}

struct FakeCapture {
    left: Option<usize>,
    read_error: Option<&'static str>,
    open_now: Arc<AtomicUsize>,
}

impl Microphone for FakeMic {
    fn open(&self) -> io::Result<Box<dyn Capture>> {
        if self.fail {
            return Err(io::Error::other("no microphone"));
        }
        self.opened.fetch_add(1, Ordering::SeqCst);
        self.open_now.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeCapture {
            left: self.chunks,
            read_error: self.read_error,
            open_now: Arc::clone(&self.open_now),
        }))
    }
}

impl Capture for FakeCapture {
    fn read(&mut self, buf: &mut [i16]) -> io::Result<usize> {
        if let Some(message) = self.read_error {
            return Err(io::Error::other(message));
        }
        if let Some(left) = &mut self.left {
            if *left == 0 {
                return Ok(0);
            }
            *left -= 1;
        }
        thread::sleep(Duration::from_millis(5));
        buf.fill(8_000);
        Ok(buf.len())
    }
}

impl Drop for FakeCapture {
    fn drop(&mut self) {
        self.open_now.fetch_sub(1, Ordering::SeqCst);
    }
}

/// An engine that replays a script: each event once it has received that many samples, then
/// `on_finish` when the audio ends, then `Ended` unless `hold_end`, after `exit_delay`.
#[derive(Default)]
struct FakeEngine {
    script: Vec<(usize, EngineEvent)>,
    on_finish: Vec<EngineEvent>,
    hold_end: bool,
    /// How long the engine takes to exit once its last event is sent, like crispasr unloading
    /// its model.
    exit_delay: Duration,
    fail: bool,
    /// Only the first dictation hears the script: the next ones are silent.
    first_only: bool,
    opens: AtomicUsize,
    sink: Mutex<Option<Arc<Sink>>>,
}

struct FakeInput {
    script: VecDeque<(usize, EngineEvent)>,
    on_finish: Vec<EngineEvent>,
    hold_end: bool,
    exit_delay: Duration,
    received: usize,
    sink: Arc<Sink>,
}

impl FakeEngine {
    fn emit(&self, event: EngineEvent) {
        let sink = self.sink.lock().unwrap().clone().expect("opened");
        sink(event);
    }
}

impl Engine for FakeEngine {
    fn open(&self, sink: Sink) -> io::Result<Box<dyn EngineInput>> {
        if self.fail {
            return Err(io::Error::other("model not found"));
        }
        let sink = Arc::new(sink);
        *self.sink.lock().unwrap() = Some(Arc::clone(&sink));
        let silent = self.first_only && self.opens.fetch_add(1, Ordering::SeqCst) > 0;
        let mut input = FakeInput {
            script: if silent {
                VecDeque::new()
            } else {
                self.script.clone().into()
            },
            on_finish: if silent {
                Vec::new()
            } else {
                self.on_finish.clone()
            },
            hold_end: self.hold_end,
            exit_delay: self.exit_delay,
            received: 0,
            sink,
        };
        input.write(&[])?;
        Ok(Box::new(input))
    }
}

impl EngineInput for FakeInput {
    fn write(&mut self, pcm: &[i16]) -> io::Result<()> {
        self.received += pcm.len();
        while self
            .script
            .front()
            .is_some_and(|(at, _)| *at <= self.received)
        {
            let (_, event) = self.script.pop_front().unwrap();
            (self.sink)(event);
        }
        Ok(())
    }

    fn finish(self: Box<Self>) {
        for event in self.on_finish {
            (self.sink)(event);
        }
        thread::sleep(self.exit_delay);
        if !self.hold_end {
            (self.sink)(EngineEvent::Ended(Ok(())));
        }
    }
}

/// An engine that takes its time to load, like crispasr: it takes audio at once (the pipe holds
/// it), says `Ready` once loaded, and its final tells how many samples it received in all.
struct SlowEngine {
    load: Duration,
    /// The model is not loaded before this many samples were received, however long `load` is:
    /// a loaded machine can starve the microphone thread for longer than the load takes.
    ready_after: usize,
    /// The samples received when `Ready` was said.
    before_ready: Arc<AtomicUsize>,
}

struct SlowInput {
    loaded: thread::JoinHandle<()>,
    received: Arc<AtomicUsize>,
    sink: Arc<Sink>,
}

impl Engine for SlowEngine {
    fn open(&self, sink: Sink) -> io::Result<Box<dyn EngineInput>> {
        let sink = Arc::new(sink);
        let received = Arc::new(AtomicUsize::new(0));
        let (load, ready_after) = (self.load, self.ready_after);
        let before_ready = Arc::clone(&self.before_ready);
        let (said, count) = (Arc::clone(&sink), Arc::clone(&received));
        let loaded = thread::spawn(move || {
            thread::sleep(load);
            let deadline = Instant::now() + WAIT;
            while count.load(Ordering::SeqCst) < ready_after && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            before_ready.store(count.load(Ordering::SeqCst), Ordering::SeqCst);
            said(EngineEvent::Ready);
        });
        Ok(Box::new(SlowInput {
            loaded,
            received,
            sink,
        }))
    }
}

impl EngineInput for SlowInput {
    fn write(&mut self, pcm: &[i16]) -> io::Result<()> {
        self.received.fetch_add(pcm.len(), Ordering::SeqCst);
        Ok(())
    }

    // Like crispasr, the audio given while loading is transcribed once the model is there.
    fn finish(self: Box<Self>) {
        self.loaded.join().unwrap();
        let received = self.received.load(Ordering::SeqCst);
        (self.sink)(final_(1, &format!("{received} samples")));
        (self.sink)(EngineEvent::Ended(Ok(())));
    }
}

fn partial(utterance: u64, text: &str) -> EngineEvent {
    EngineEvent::Partial {
        utterance,
        text: text.into(),
    }
}

fn final_(utterance: u64, text: &str) -> EngineEvent {
    EngineEvent::Final {
        utterance,
        text: text.into(),
    }
}

/// A watcher's end of the stream, as the events it received.
struct Watcher {
    events: mpsc::Receiver<Event>,
}

struct LineWriter {
    pending: Vec<u8>,
    events: mpsc::Sender<Event>,
}

impl Write for LineWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let event = serde_json::from_slice(&line).expect("each line is an event");
            let _ = self.events.send(event);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Watcher {
    fn on(daemon: &Handle) -> Watcher {
        let (tx, rx) = mpsc::channel();
        daemon.watch(Box::new(LineWriter {
            pending: Vec::new(),
            events: tx,
        }));
        Watcher { events: rx }
    }

    /// The next event that is not a level.
    fn next(&self) -> Event {
        loop {
            match self.events.recv_timeout(WAIT).expect("an event") {
                Event::Level { .. } => continue,
                event => return event,
            }
        }
    }

    fn expect(&self, expected: &[Event]) {
        let got: Vec<Event> = expected.iter().map(|_| self.next()).collect();
        assert_eq!(got, expected);
    }

    fn skip_to(&self, wanted: &Event) {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if self.next() == *wanted {
                return;
            }
        }
        panic!("{wanted:?} never came");
    }
}

fn state(state: State) -> Event {
    Event::State { state }
}

fn ev_partial(utterance: u64, text: &str) -> Event {
    Event::Partial {
        utterance,
        text: text.into(),
    }
}

fn ev_final(utterance: u64, text: &str) -> Event {
    Event::Final {
        utterance,
        text: text.into(),
    }
}

fn daemon(mic: FakeMic, engine: FakeEngine) -> (Handle, Arc<FakeMic>, Arc<FakeEngine>) {
    let mic = Arc::new(mic);
    let engine = Arc::new(engine);
    let handle = spawn(
        mic.clone(),
        engine.clone(),
        None,
        HOLD,
        Arc::new(Instant::now),
        no_vocabulary(),
        Vec::new(),
    );
    (handle, mic, engine)
}

/// What the daemon handed to the output.
#[derive(Debug, Clone, PartialEq)]
enum Handed {
    Begin,
    Phrase(String),
    Enter,
}

/// What the daemon handed over, and the class of the window the fake desktop has focused.
#[derive(Clone, Default)]
struct FakePaste(Arc<Mutex<Vec<Handed>>>, Arc<Mutex<Option<String>>>);

impl FakePaste {
    /// A fake output whose focused window is of class `class`.
    fn in_window(class: &str) -> FakePaste {
        let paste = FakePaste::default();
        *paste.1.lock().unwrap() = Some(class.into());
        paste
    }
}

impl Paste for FakePaste {
    fn begin(&self) -> Option<String> {
        self.0.lock().unwrap().push(Handed::Begin);
        self.1.lock().unwrap().clone()
    }

    fn phrase(&self, text: String) {
        self.0.lock().unwrap().push(Handed::Phrase(text));
    }

    fn enter(&self) {
        self.0.lock().unwrap().push(Handed::Enter);
    }
}

/// A daemon that pastes into a fake output.
fn pasting_daemon(engine: FakeEngine) -> (Handle, FakePaste) {
    let paste = FakePaste::default();
    let handle = spawn(
        Arc::new(FakeMic::default()),
        Arc::new(engine),
        Some(Box::new(paste.clone())),
        HOLD,
        Arc::new(Instant::now),
        no_vocabulary(),
        Vec::new(),
    );
    (handle, paste)
}

fn two_phrases() -> FakeEngine {
    FakeEngine {
        script: vec![
            (0, EngineEvent::Ready),
            (1_600, partial(1, "Bonjour")),
            (3_200, final_(1, "Bonjour.")),
            (4_800, partial(2, "Ça")),
        ],
        on_finish: vec![final_(2, "Ça va ?")],
        ..FakeEngine::default()
    }
}

/// A vocabulary file that does not exist: nothing is corrected.
fn no_vocabulary() -> PathBuf {
    PathBuf::from("/nonexistent/vocaline/vocabulary.txt")
}

const HOLD: Duration = Duration::from_millis(250);

/// A clock that only moves when a test says so: no test waits for the 250 ms of a hold.
#[derive(Clone)]
struct FakeClock {
    start: Instant,
    elapsed: Arc<Mutex<Duration>>,
}

impl FakeClock {
    fn new() -> FakeClock {
        FakeClock {
            start: Instant::now(),
            elapsed: Arc::default(),
        }
    }

    fn advance(&self, by: Duration) {
        *self.elapsed.lock().unwrap() += by;
    }

    fn clock(&self) -> Clock {
        let this = self.clone();
        Arc::new(move || this.start + *this.elapsed.lock().unwrap())
    }
}

/// A daemon with the fake clock, for the commands of a key.
fn keyed_daemon(engine: FakeEngine) -> (Handle, FakeClock, Arc<FakeMic>) {
    let clock = FakeClock::new();
    let mic = Arc::new(FakeMic::default());
    let handle = spawn(
        mic.clone(),
        Arc::new(engine),
        None,
        HOLD,
        clock.clock(),
        no_vocabulary(),
        Vec::new(),
    );
    (handle, clock, mic)
}

/// A daemon with the fake clock and a fake output, for a key that pastes.
fn keyed_pasting_daemon(engine: FakeEngine) -> (Handle, FakeClock, FakePaste) {
    let clock = FakeClock::new();
    let paste = FakePaste::default();
    let handle = spawn(
        Arc::new(FakeMic::default()),
        Arc::new(engine),
        Some(Box::new(paste.clone())),
        HOLD,
        clock.clock(),
        no_vocabulary(),
        Vec::new(),
    );
    (handle, clock, paste)
}

/// Two phrases closed by their pauses, nothing left open when the audio ends.
fn two_closed_phrases() -> FakeEngine {
    FakeEngine {
        script: vec![
            (0, EngineEvent::Ready),
            (1_600, final_(1, "Bonjour.")),
            (3_200, final_(2, "Ça va ?")),
        ],
        ..FakeEngine::default()
    }
}

fn ready_engine() -> FakeEngine {
    FakeEngine {
        script: vec![(0, EngineEvent::Ready)],
        ..FakeEngine::default()
    }
}

fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "{what}");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_watcher_first_gets_the_version_and_the_current_state() {
    let (daemon, _, _) = daemon(FakeMic::default(), ready_engine());
    let watcher = Watcher::on(&daemon);
    watcher.expect(&[Event::Hello { version: 1 }, state(State::Idle)]);
}

#[test]
fn the_daemon_starts_idle() {
    let (daemon, mic, _) = daemon(FakeMic::default(), ready_engine());
    assert_eq!(daemon.command(Command::Status), Ok(State::Idle));
    assert_eq!(mic.opened.load(Ordering::SeqCst), 0);
}

#[test]
fn start_goes_through_loading_to_listening() {
    let engine = FakeEngine {
        script: vec![(3_200, EngineEvent::Ready)],
        ..FakeEngine::default()
    };
    let (daemon, mic, _) = daemon(FakeMic::default(), engine);
    let watcher = Watcher::on(&daemon);
    watcher.expect(&[Event::Hello { version: 1 }, state(State::Idle)]);
    assert_eq!(daemon.command(Command::Start), Ok(State::Loading));
    watcher.expect(&[state(State::Loading), state(State::Listening)]);
    assert_eq!(daemon.command(Command::Status), Ok(State::Listening));
    assert_eq!(mic.opened.load(Ordering::SeqCst), 1);
}

#[test]
fn partials_and_finals_reach_the_watchers() {
    let engine = FakeEngine {
        script: vec![
            (0, EngineEvent::Ready),
            (1_600, EngineEvent::Silence),
            (3_200, partial(1, "Bonjour")),
            (4_800, partial(1, "Bonjour, je")),
            (6_400, final_(1, "Bonjour, je parle.")),
        ],
        ..FakeEngine::default()
    };
    let (daemon, _, _) = daemon(FakeMic::default(), engine);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&state(State::Listening));
    watcher.expect(&[
        Event::Silence,
        ev_partial(1, "Bonjour"),
        ev_partial(1, "Bonjour, je"),
        ev_final(1, "Bonjour, je parle."),
    ]);
}

// Seen live: two readings of the same words won in turn for dozens of steps.
#[test]
fn a_partial_read_two_ways_in_turn_does_not_blink() {
    let engine = FakeEngine {
        script: vec![
            (0, EngineEvent::Ready),
            (1_600, partial(1, "une personne de manière")),
            (3_200, partial(1, "une personne. de manière simple")),
            (4_800, partial(1, "une personne de manière simple, avec")),
            (
                6_400,
                partial(1, "une personne. de manière simple, avec son"),
            ),
            (
                8_000,
                final_(1, "une personne de manière simple, avec son nom."),
            ),
            (9_600, partial(2, "Quand")),
        ],
        ..FakeEngine::default()
    };
    let (daemon, _, _) = daemon(FakeMic::default(), engine);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&state(State::Listening));
    watcher.expect(&[
        ev_partial(1, "une personne de manière"),
        ev_partial(1, "une personne de manière simple"),
        ev_partial(1, "une personne de manière simple, avec"),
        ev_partial(1, "une personne de manière simple, avec son"),
        ev_final(1, "une personne de manière simple, avec son nom."),
        ev_partial(2, "Quand"),
    ]);
}

#[test]
fn levels_are_published_while_listening() {
    let (daemon, _, _) = daemon(FakeMic::default(), ready_engine());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "no level");
        if let Event::Level { rms } = watcher.events.recv_timeout(WAIT).unwrap() {
            assert!((rms - 0.244).abs() < 0.001, "{rms}");
            break;
        }
    }
}

#[test]
fn stop_finalises_the_open_phrase() {
    let engine = FakeEngine {
        script: vec![(0, EngineEvent::Ready), (1_600, partial(1, "Je dis"))],
        on_finish: vec![final_(1, "Je dis ceci.")],
        ..FakeEngine::default()
    };
    let (daemon, mic, _) = daemon(FakeMic::default(), engine);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_partial(1, "Je dis"));
    assert_eq!(daemon.command(Command::Stop), Ok(State::Stopping));
    watcher.expect(&[
        state(State::Stopping),
        ev_final(1, "Je dis ceci."),
        state(State::Idle),
    ]);
    wait_until("the microphone is closed", || {
        mic.open_now.load(Ordering::SeqCst) == 0
    });
}

#[test]
fn toggle_starts_then_stops() {
    let (daemon, _, _) = daemon(FakeMic::default(), ready_engine());
    let watcher = Watcher::on(&daemon);
    assert_eq!(daemon.command(Command::Toggle), Ok(State::Loading));
    watcher.skip_to(&state(State::Listening));
    assert_eq!(daemon.command(Command::Toggle), Ok(State::Stopping));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(daemon.command(Command::Status), Ok(State::Idle));
}

#[test]
fn toggle_while_loading_stops() {
    let engine = FakeEngine {
        hold_end: true,
        ..FakeEngine::default()
    };
    let (daemon, _, _) = daemon(FakeMic::default(), engine);
    assert_eq!(daemon.command(Command::Toggle), Ok(State::Loading));
    assert_eq!(daemon.command(Command::Toggle), Ok(State::Stopping));
}

#[test]
fn start_while_listening_changes_nothing() {
    let (daemon, mic, _) = daemon(FakeMic::default(), ready_engine());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&state(State::Listening));
    assert_eq!(daemon.command(Command::Start), Ok(State::Listening));
    assert_eq!(mic.opened.load(Ordering::SeqCst), 1);
}

#[test]
fn stop_when_idle_changes_nothing() {
    let (daemon, _, _) = daemon(FakeMic::default(), ready_engine());
    assert_eq!(daemon.command(Command::Stop), Ok(State::Idle));
}

#[test]
fn start_while_stopping_is_refused_until_the_engine_has_ended() {
    let engine = FakeEngine {
        script: vec![(0, EngineEvent::Ready)],
        hold_end: true,
        ..FakeEngine::default()
    };
    let (daemon, mic, engine) = daemon(FakeMic::default(), engine);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&state(State::Listening));
    daemon.command(Command::Stop).unwrap();
    assert!(daemon.command(Command::Start).is_err());
    assert!(daemon.command(Command::Toggle).is_err());
    assert_eq!(daemon.command(Command::Status), Ok(State::Stopping));
    engine.emit(EngineEvent::Ended(Ok(())));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(daemon.command(Command::Start), Ok(State::Loading));
    assert_eq!(mic.opened.load(Ordering::SeqCst), 2);
}

#[test]
fn an_engine_that_fails_while_listening_returns_to_idle_with_an_error() {
    let engine = FakeEngine {
        script: vec![
            (0, EngineEvent::Ready),
            (1_600, EngineEvent::Ended(Err("the engine stopped".into()))),
        ],
        ..FakeEngine::default()
    };
    let (daemon, mic, _) = daemon(FakeMic::default(), engine);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&state(State::Listening));
    watcher.expect(&[
        Event::Error {
            message: "the engine stopped".into(),
        },
        state(State::Idle),
    ]);
    wait_until("the microphone is closed", || {
        mic.open_now.load(Ordering::SeqCst) == 0
    });
}

#[test]
fn an_engine_that_cannot_start_leaves_the_daemon_idle() {
    let engine = FakeEngine {
        fail: true,
        ..FakeEngine::default()
    };
    let (daemon, mic, _) = daemon(FakeMic::default(), engine);
    let watcher = Watcher::on(&daemon);
    watcher.expect(&[Event::Hello { version: 1 }, state(State::Idle)]);
    let error = daemon.command(Command::Start).unwrap_err();
    assert!(error.contains("model not found"), "{error}");
    assert!(matches!(watcher.next(), Event::Error { .. }));
    assert_eq!(daemon.command(Command::Status), Ok(State::Idle));
    assert_eq!(mic.open_now.load(Ordering::SeqCst), 0);
}

#[test]
fn a_microphone_that_cannot_open_leaves_the_daemon_idle() {
    let mic = FakeMic {
        fail: true,
        ..FakeMic::default()
    };
    let (daemon, _, engine) = daemon(mic, ready_engine());
    let error = daemon.command(Command::Start).unwrap_err();
    assert!(error.contains("no microphone"), "{error}");
    assert_eq!(daemon.command(Command::Status), Ok(State::Idle));
    assert!(engine.sink.lock().unwrap().is_none(), "no engine loaded");
}

#[test]
fn a_microphone_that_fails_at_once_says_why_then_the_daemon_returns_to_idle() {
    let mic = FakeMic {
        read_error: Some("pw-record stopped (exit status: 1): connection refused"),
        ..FakeMic::default()
    };
    let (daemon, mic, _) = daemon(mic, ready_engine());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&Event::Error {
        message: "the microphone failed: pw-record stopped (exit status: 1): connection refused"
            .into(),
    });
    watcher.expect(&[state(State::Stopping), state(State::Idle)]);
    wait_until("the microphone is closed", || {
        mic.open_now.load(Ordering::SeqCst) == 0
    });
}

#[test]
fn when_the_audio_ends_the_dictation_stops_by_itself() {
    let engine = FakeEngine {
        script: vec![(0, EngineEvent::Ready), (1_600, partial(1, "Fin"))],
        on_finish: vec![final_(1, "Fin.")],
        ..FakeEngine::default()
    };
    let mic = FakeMic {
        chunks: Some(3),
        ..FakeMic::default()
    };
    let (daemon, _, _) = daemon(mic, engine);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_partial(1, "Fin"));
    watcher.expect(&[
        state(State::Stopping),
        ev_final(1, "Fin."),
        state(State::Idle),
    ]);
}

fn slow_daemon(
    load: Duration,
    ready_after: usize,
    mic: FakeMic,
) -> (Handle, Arc<AtomicUsize>, FakePaste) {
    let before_ready = Arc::new(AtomicUsize::new(0));
    let engine = SlowEngine {
        load,
        ready_after,
        before_ready: Arc::clone(&before_ready),
    };
    let paste = FakePaste::default();
    let handle = spawn(
        Arc::new(mic),
        Arc::new(engine),
        Some(Box::new(paste.clone())),
        HOLD,
        Arc::new(Instant::now),
        no_vocabulary(),
        Vec::new(),
    );
    (handle, before_ready, paste)
}

// The model takes a moment to load, the microphone does not wait for it: the words spoken in
// that moment are in the transcript, not dropped.
#[test]
fn words_spoken_while_the_model_loads_reach_the_transcript() {
    // 3 s of speech, handed over in 150 ms: all of it comes before the model is ready.
    let mic = FakeMic {
        chunks: Some(30),
        ..FakeMic::default()
    };
    let (daemon, before_ready, paste) = slow_daemon(Duration::from_millis(300), 48_000, mic);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&state(State::Loading));
    watcher.expect(&[
        state(State::Stopping),
        ev_final(1, "48000 samples"),
        state(State::Idle),
    ]);
    assert_eq!(before_ready.load(Ordering::SeqCst), 48_000);
    assert_eq!(
        *paste.0.lock().unwrap(),
        [Handed::Begin, Handed::Phrase("48000 samples".into())]
    );
}

#[test]
fn speech_goes_on_after_the_model_is_ready_and_nothing_before_it_is_lost() {
    // 10 s of speech over 500 ms: the model is ready, 200 ms in, with speech on both sides.
    let mic = FakeMic {
        chunks: Some(100),
        ..FakeMic::default()
    };
    let (daemon, before_ready, _) = slow_daemon(Duration::from_millis(200), 0, mic);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&state(State::Loading));
    watcher.expect(&[
        state(State::Listening),
        state(State::Stopping),
        ev_final(1, "160000 samples"),
    ]);
    // A loaded machine reads fewer than the 40 chunks 200 ms is worth, never none.
    assert!(
        before_ready.load(Ordering::SeqCst) >= 1_600,
        "nothing came before Ready"
    );
}

#[test]
fn utterance_numbers_keep_growing_across_dictations() {
    // The engine counts from 1 in each stream; the stream of the daemon does not start over.
    let engine = FakeEngine {
        script: vec![
            (0, EngineEvent::Ready),
            (1_600, final_(1, "Un.")),
            (3_200, final_(2, "Deux.")),
        ],
        ..FakeEngine::default()
    };
    let (daemon, _, _) = daemon(FakeMic::default(), engine);
    let watcher = Watcher::on(&daemon);
    for expected in [[1, 2], [3, 4]] {
        daemon.command(Command::Start).unwrap();
        watcher.skip_to(&ev_final(expected[0], "Un."));
        watcher.expect(&[ev_final(expected[1], "Deux.")]);
        daemon.command(Command::Stop).unwrap();
        watcher.skip_to(&state(State::Idle));
    }
}

#[test]
fn every_watcher_gets_every_event() {
    let engine = FakeEngine {
        script: vec![(0, EngineEvent::Ready), (1_600, final_(1, "Pour tous."))],
        ..FakeEngine::default()
    };
    let (daemon, _, _) = daemon(FakeMic::default(), engine);
    let watchers = [Watcher::on(&daemon), Watcher::on(&daemon)];
    daemon.command(Command::Start).unwrap();
    for watcher in &watchers {
        watcher.skip_to(&ev_final(1, "Pour tous."));
    }
}

fn socket_path(name: &str) -> PathBuf {
    let folder = std::env::temp_dir().join(format!("vocaline-daemon-{}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap();
    let path = folder.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

fn ask(path: &Path, line: &str) -> String {
    let mut stream = UnixStream::connect(path).unwrap();
    stream.write_all(format!("{line}\n").as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    answer
}

#[test]
fn commands_and_the_stream_go_through_the_socket() {
    let path = socket_path("commands.sock");
    let (daemon, _, _) = daemon(FakeMic::default(), ready_engine());
    let listener = bind(&path).unwrap();
    thread::spawn(move || serve(listener, daemon));

    assert_eq!(ask(&path, "status"), "{\"state\":\"idle\"}\n");
    let watch = UnixStream::connect(&path).unwrap();
    (&watch).write_all(b"watch\n").unwrap();
    let mut lines = BufReader::new(watch).lines();
    assert_eq!(
        lines.next().unwrap().unwrap(),
        r#"{"type":"hello","version":1}"#
    );
    assert_eq!(
        lines.next().unwrap().unwrap(),
        r#"{"type":"state","state":"idle"}"#
    );

    assert_eq!(ask(&path, "start"), "{\"state\":\"loading\"}\n");
    assert_eq!(
        lines.next().unwrap().unwrap(),
        r#"{"type":"state","state":"loading"}"#
    );
    let answer = ask(&path, "dance");
    assert!(answer.starts_with("{\"error\":"), "{answer}");
}

#[test]
fn a_second_daemon_on_the_same_socket_is_refused() {
    let path = socket_path("second.sock");
    let listener = bind(&path).unwrap();
    let error = bind(&path).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    drop(listener);
}

#[test]
fn a_socket_left_by_a_dead_daemon_is_replaced() {
    let path = socket_path("stale.sock");
    drop(UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    assert!(bind(&path).is_ok());
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn hold_listens_from_the_press_to_the_release_then_the_phrase_is_final() {
    let engine = FakeEngine {
        script: vec![(0, EngineEvent::Ready), (1_600, partial(1, "Je dis"))],
        on_finish: vec![final_(1, "Je dis ceci.")],
        ..FakeEngine::default()
    };
    let (daemon, clock, mic) = keyed_daemon(engine);
    let watcher = Watcher::on(&daemon);
    assert_eq!(daemon.command(Command::Press), Ok(State::Loading));
    watcher.skip_to(&ev_partial(1, "Je dis"));
    clock.advance(ms(2_000));
    assert_eq!(daemon.command(Command::Release), Ok(State::Stopping));
    watcher.expect(&[
        state(State::Stopping),
        ev_final(1, "Je dis ceci."),
        state(State::Idle),
    ]);
    wait_until("the microphone is closed", || {
        mic.open_now.load(Ordering::SeqCst) == 0
    });
}

#[test]
fn a_release_at_the_threshold_stops_and_a_shorter_one_does_not() {
    let (daemon, clock, _) = keyed_daemon(ready_engine());
    daemon.command(Command::Press).unwrap();
    clock.advance(ms(249));
    // The fake engine may already be ready: what matters is that it is not stopping.
    let reached = daemon.command(Command::Release).unwrap();
    assert!(
        matches!(reached, State::Loading | State::Listening),
        "{reached:?}"
    );

    let (daemon, clock, _) = keyed_daemon(ready_engine());
    daemon.command(Command::Press).unwrap();
    clock.advance(ms(250));
    assert_eq!(daemon.command(Command::Release), Ok(State::Stopping));
}

#[test]
fn tap_then_tap_keeps_listening_then_stops() {
    let (daemon, clock, _) = keyed_daemon(ready_engine());
    let watcher = Watcher::on(&daemon);
    assert_eq!(daemon.command(Command::Press), Ok(State::Loading));
    clock.advance(ms(80));
    daemon.command(Command::Release).unwrap();
    watcher.skip_to(&state(State::Listening));
    assert_eq!(daemon.command(Command::Status), Ok(State::Listening));

    clock.advance(ms(10_000));
    assert_eq!(daemon.command(Command::Press), Ok(State::Stopping));
    // The release of the tap that stopped it must not stop anything else.
    clock.advance(ms(80));
    daemon.command(Command::Release).unwrap();
    watcher.skip_to(&state(State::Idle));
    assert_eq!(daemon.command(Command::Status), Ok(State::Idle));
}

#[test]
fn a_long_press_that_stops_a_dictation_does_not_start_another_on_release() {
    let (daemon, clock, mic) = keyed_daemon(ready_engine());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Press).unwrap();
    clock.advance(ms(50));
    daemon.command(Command::Release).unwrap();
    watcher.skip_to(&state(State::Listening));
    daemon.command(Command::Press).unwrap();
    clock.advance(ms(2_000));
    daemon.command(Command::Release).unwrap();
    watcher.skip_to(&state(State::Idle));
    assert_eq!(daemon.command(Command::Status), Ok(State::Idle));
    assert_eq!(mic.opened.load(Ordering::SeqCst), 1);
}

#[test]
fn a_press_while_loading_stops_without_a_second_start() {
    let engine = FakeEngine {
        hold_end: true,
        ..FakeEngine::default()
    };
    let (daemon, clock, mic) = keyed_daemon(engine);
    assert_eq!(daemon.command(Command::Press), Ok(State::Loading));
    clock.advance(ms(80));
    daemon.command(Command::Release).unwrap();
    assert_eq!(daemon.command(Command::Status), Ok(State::Loading));
    assert_eq!(daemon.command(Command::Press), Ok(State::Stopping));
    assert_eq!(mic.opened.load(Ordering::SeqCst), 1);
}

#[test]
fn a_release_without_a_press_does_nothing() {
    let (daemon, clock, mic) = keyed_daemon(ready_engine());
    clock.advance(ms(5_000));
    assert_eq!(daemon.command(Command::Release), Ok(State::Idle));
    assert_eq!(mic.opened.load(Ordering::SeqCst), 0);

    // Listening, started by `start`: a stray release is not a hold.
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&state(State::Listening));
    clock.advance(ms(5_000));
    assert_eq!(daemon.command(Command::Release), Ok(State::Listening));
}

#[test]
fn a_press_while_stopping_is_refused_and_its_release_does_nothing() {
    let engine = FakeEngine {
        script: vec![(0, EngineEvent::Ready)],
        hold_end: true,
        ..FakeEngine::default()
    };
    let (daemon, clock, _) = keyed_daemon(engine);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&state(State::Listening));
    daemon.command(Command::Stop).unwrap();
    assert!(daemon.command(Command::Press).is_err());
    clock.advance(ms(2_000));
    assert_eq!(daemon.command(Command::Release), Ok(State::Stopping));
}

#[test]
fn a_press_that_cannot_start_leaves_the_release_without_effect() {
    let clock = FakeClock::new();
    let mic = Arc::new(FakeMic {
        fail: true,
        ..FakeMic::default()
    });
    let daemon = spawn(
        mic,
        Arc::new(ready_engine()),
        None,
        HOLD,
        clock.clock(),
        no_vocabulary(),
        Vec::new(),
    );
    assert!(daemon.command(Command::Press).is_err());
    clock.advance(ms(2_000));
    assert_eq!(daemon.command(Command::Release), Ok(State::Idle));
}

#[test]
fn start_stop_and_toggle_still_work_beside_a_key() {
    let (daemon, clock, _) = keyed_daemon(ready_engine());
    let watcher = Watcher::on(&daemon);
    assert_eq!(daemon.command(Command::Toggle), Ok(State::Loading));
    watcher.skip_to(&state(State::Listening));
    clock.advance(ms(1_000));
    assert_eq!(daemon.command(Command::Stop), Ok(State::Stopping));
    watcher.skip_to(&state(State::Idle));
}

#[test]
fn press_and_release_go_through_the_socket() {
    let path = socket_path("keys.sock");
    let (daemon, clock, _) = keyed_daemon(ready_engine());
    let listener = bind(&path).unwrap();
    thread::spawn(move || serve(listener, daemon));

    assert_eq!(ask(&path, "press"), "{\"state\":\"loading\"}\n");
    clock.advance(ms(300));
    assert_eq!(ask(&path, "release"), "{\"state\":\"stopping\"}\n");
}

#[test]
fn nothing_is_pasted_while_the_key_is_held() {
    let (daemon, clock, paste) = keyed_pasting_daemon(two_closed_phrases());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Press).unwrap();
    watcher.skip_to(&ev_final(2, "Ça va ?"));
    clock.advance(ms(2_000));
    // The stream has both finals; the window has nothing yet.
    assert_eq!(*paste.0.lock().unwrap(), [Handed::Begin]);
}

#[test]
fn the_whole_dictation_is_pasted_once_on_release() {
    let (daemon, clock, paste) = keyed_pasting_daemon(two_closed_phrases());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Press).unwrap();
    watcher.skip_to(&ev_final(2, "Ça va ?"));
    clock.advance(ms(2_000));
    assert_eq!(daemon.command(Command::Release), Ok(State::Stopping));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(
        *paste.0.lock().unwrap(),
        [Handed::Begin, Handed::Phrase("Bonjour. Ça va ?".into())]
    );
}

#[test]
fn a_tapped_dictation_is_pasted_once_on_the_second_tap() {
    let (daemon, clock, paste) = keyed_pasting_daemon(two_closed_phrases());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Press).unwrap();
    clock.advance(ms(80));
    daemon.command(Command::Release).unwrap();
    watcher.skip_to(&ev_final(2, "Ça va ?"));
    assert_eq!(*paste.0.lock().unwrap(), [Handed::Begin]);

    clock.advance(ms(10_000));
    assert_eq!(daemon.command(Command::Press), Ok(State::Stopping));
    clock.advance(ms(80));
    daemon.command(Command::Release).unwrap();
    watcher.skip_to(&state(State::Idle));
    assert_eq!(
        *paste.0.lock().unwrap(),
        [Handed::Begin, Handed::Phrase("Bonjour. Ça va ?".into())]
    );
}

#[test]
fn the_last_utterance_open_at_the_stop_is_in_the_paste() {
    let (daemon, paste) = pasting_daemon(two_phrases());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_partial(2, "Ça"));
    assert_eq!(*paste.0.lock().unwrap(), [Handed::Begin]);
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&state(State::Idle));
    assert_eq!(
        *paste.0.lock().unwrap(),
        [Handed::Begin, Handed::Phrase("Bonjour. Ça va ?".into())]
    );
}

/// An engine that has sent its last final and says so, then takes a second to exit.
fn slow_to_exit() -> FakeEngine {
    FakeEngine {
        on_finish: vec![final_(1, "Bonjour."), EngineEvent::Flushed],
        exit_delay: ms(1_000),
        ..FakeEngine::default()
    }
}

/// Waits for the output to be handed a phrase, and tells when that was.
fn pasted_within(paste: &FakePaste, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if paste
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|handed| matches!(handed, Handed::Phrase(_)))
        {
            return true;
        }
        thread::sleep(ms(5));
    }
    false
}

#[test]
fn the_paste_does_not_wait_for_the_engine_to_exit() {
    let (daemon, paste) = pasting_daemon(slow_to_exit());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    daemon.command(Command::Stop).unwrap();
    assert!(
        pasted_within(&paste, ms(500)),
        "the engine had said its last final; the paste waited for its exit"
    );
    // The model is still being unloaded, so the daemon is not idle yet.
    assert_eq!(daemon.command(Command::Status), Ok(State::Stopping));
    watcher.skip_to(&state(State::Stopping));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(
        *paste.0.lock().unwrap(),
        [Handed::Begin, Handed::Phrase("Bonjour.".into())]
    );
}

#[test]
fn a_dictation_cannot_start_while_the_engine_is_still_exiting_after_the_paste() {
    let (daemon, paste) = pasting_daemon(slow_to_exit());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    daemon.command(Command::Stop).unwrap();
    assert!(pasted_within(&paste, ms(500)));
    assert!(daemon.command(Command::Start).is_err());
    watcher.skip_to(&state(State::Stopping));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(daemon.command(Command::Start), Ok(State::Loading));
}

#[test]
fn an_engine_that_never_says_it_flushed_is_pasted_when_it_ends() {
    let engine = FakeEngine {
        on_finish: vec![final_(1, "Bonjour.")],
        exit_delay: ms(600),
        ..FakeEngine::default()
    };
    let (daemon, paste) = pasting_daemon(engine);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    daemon.command(Command::Stop).unwrap();
    assert!(!pasted_within(&paste, ms(200)));
    watcher.skip_to(&state(State::Stopping));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(
        *paste.0.lock().unwrap(),
        [Handed::Begin, Handed::Phrase("Bonjour.".into())]
    );
}

#[test]
fn an_empty_engine_line_while_listening_pastes_nothing() {
    let engine = FakeEngine {
        script: vec![
            (0, EngineEvent::Ready),
            (1_600, final_(1, "Bonjour.")),
            (3_200, EngineEvent::Flushed),
            (4_800, partial(2, "Ça")),
        ],
        on_finish: vec![final_(2, "Ça va ?")],
        ..FakeEngine::default()
    };
    let (daemon, paste) = pasting_daemon(engine);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_partial(2, "Ça"));
    assert_eq!(*paste.0.lock().unwrap(), [Handed::Begin]);
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&state(State::Idle));
    // One paste, with both phrases: the stray flush did not cut the dictation in two.
    assert_eq!(
        *paste.0.lock().unwrap(),
        [Handed::Begin, Handed::Phrase("Bonjour. Ça va ?".into())]
    );
}

#[test]
fn a_dictation_with_no_final_pastes_nothing() {
    let (daemon, paste) = pasting_daemon(ready_engine());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&state(State::Listening));
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&state(State::Idle));
    assert_eq!(*paste.0.lock().unwrap(), [Handed::Begin]);
}

#[test]
fn a_dictation_stopped_while_loading_pastes_nothing_of_the_previous_one() {
    let engine = FakeEngine {
        script: vec![(0, EngineEvent::Ready), (1_600, final_(1, "Bonjour."))],
        first_only: true,
        ..FakeEngine::default()
    };
    let (daemon, paste) = pasting_daemon(engine);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_final(1, "Bonjour."));
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&state(State::Idle));

    // The engine of the second dictation never says it is ready.
    daemon.command(Command::Start).unwrap();
    assert_eq!(daemon.command(Command::Stop), Ok(State::Stopping));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(daemon.command(Command::Status), Ok(State::Idle));
    assert_eq!(
        *paste.0.lock().unwrap(),
        [
            Handed::Begin,
            Handed::Phrase("Bonjour.".into()),
            Handed::Begin
        ]
    );
}

#[test]
fn the_finals_are_joined_by_one_space_whatever_the_engine_puts_around_them() {
    let engine = FakeEngine {
        on_finish: vec![
            final_(1, " Bonjour. "),
            final_(2, "   "),
            final_(3, " Ça va ?"),
        ],
        ..FakeEngine::default()
    };
    let (daemon, paste) = pasting_daemon(engine);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&state(State::Stopping));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(
        *paste.0.lock().unwrap(),
        [Handed::Begin, Handed::Phrase("Bonjour. Ça va ?".into())]
    );
}

#[test]
fn an_engine_that_fails_after_some_finals_still_has_them_pasted_once() {
    let engine = FakeEngine {
        on_finish: vec![
            final_(1, "Bonjour."),
            EngineEvent::Ended(Err("the engine died".into())),
        ],
        ..FakeEngine::default()
    };
    let (daemon, paste) = pasting_daemon(engine);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&state(State::Stopping));
    watcher.skip_to(&state(State::Idle));
    // The fake then ends its stream a second time, as a late duplicate: nothing more is pasted.
    assert_eq!(daemon.command(Command::Status), Ok(State::Idle));
    assert_eq!(
        *paste.0.lock().unwrap(),
        [Handed::Begin, Handed::Phrase("Bonjour.".into())]
    );
}

#[test]
fn no_paste_still_pastes_nothing() {
    let (daemon, paste) = pasting_daemon(two_phrases());
    let watcher = Watcher::on(&daemon);
    assert_eq!(daemon.command(Command::StartNoPaste), Ok(State::Loading));
    watcher.skip_to(&ev_final(1, "Bonjour."));
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&ev_final(2, "Ça va ?"));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(*paste.0.lock().unwrap(), []);
}

#[test]
fn a_dictation_after_one_without_paste_pastes_again() {
    let (daemon, paste) = pasting_daemon(ready_engine());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::StartNoPaste).unwrap();
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&state(State::Stopping));
    watcher.skip_to(&state(State::Idle));
    daemon.command(Command::Start).unwrap();
    assert_eq!(*paste.0.lock().unwrap(), [Handed::Begin]);
}

#[test]
fn start_no_paste_goes_through_the_socket() {
    let path = socket_path("no-paste.sock");
    let (daemon, paste) = pasting_daemon(ready_engine());
    let listener = bind(&path).unwrap();
    thread::spawn(move || serve(listener, daemon));

    assert_eq!(ask(&path, "start --no-paste"), "{\"state\":\"loading\"}\n");
    assert_eq!(
        ask(&path, "start --no-paste"),
        "{\"state\":\"listening\"}\n"
    );
    assert_eq!(*paste.0.lock().unwrap(), []);
}

/// A fresh vocabulary file in its own temporary folder.
fn vocabulary_file(name: &str) -> PathBuf {
    let folder =
        std::env::temp_dir().join(format!("vocaline-daemon-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    folder.join("vocabulary.txt")
}

/// A daemon that pastes into a fake output and reads its vocabulary from `file`.
fn vocal_daemon(engine: FakeEngine, file: PathBuf) -> (Handle, FakePaste) {
    let paste = FakePaste::default();
    let handle = spawn(
        Arc::new(FakeMic::default()),
        Arc::new(engine),
        Some(Box::new(paste.clone())),
        HOLD,
        Arc::new(Instant::now),
        file,
        Vec::new(),
    );
    (handle, paste)
}

/// A phrase with an entry of the vocabulary inside, partial first.
fn foo_bar_phrase() -> FakeEngine {
    FakeEngine {
        script: vec![
            (0, EngineEvent::Ready),
            (1_600, partial(1, "Ouvre foo bar")),
            (3_200, final_(1, "Ouvre foo bar.")),
        ],
        ..FakeEngine::default()
    }
}

#[test]
fn after_teach_the_partial_the_final_and_the_paste_hold_the_wanted_word() {
    let file = vocabulary_file("teach");
    crate::vocabulary::teach(&file, "foo bar", "Foobar").unwrap();
    let (daemon, paste) = vocal_daemon(foo_bar_phrase(), file);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_partial(1, "Ouvre Foobar"));
    watcher.skip_to(&ev_final(1, "Ouvre Foobar."));
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&state(State::Idle));
    assert_eq!(
        *paste.0.lock().unwrap(),
        [Handed::Begin, Handed::Phrase("Ouvre Foobar.".into())]
    );
}

#[test]
fn a_word_that_merely_contains_an_entry_is_not_changed_in_a_dictation() {
    let file = vocabulary_file("contains");
    crate::vocabulary::teach(&file, "bar", "Bar").unwrap();
    let engine = FakeEngine {
        script: vec![
            (0, EngineEvent::Ready),
            (1_600, final_(1, "Un barbare au bar.")),
        ],
        ..FakeEngine::default()
    };
    let (daemon, paste) = vocal_daemon(engine, file);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_final(1, "Un barbare au Bar."));
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&state(State::Idle));
    assert_eq!(
        *paste.0.lock().unwrap(),
        [Handed::Begin, Handed::Phrase("Un barbare au Bar.".into())]
    );
}

#[test]
fn an_entry_taught_between_two_dictations_applies_to_the_next() {
    let file = vocabulary_file("between");
    let (daemon, _) = vocal_daemon(foo_bar_phrase(), file.clone());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_final(1, "Ouvre foo bar."));
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&state(State::Idle));

    crate::vocabulary::teach(&file, "foo bar", "Foobar").unwrap();
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_final(2, "Ouvre Foobar."));
}

#[test]
fn a_line_of_the_vocabulary_that_cannot_be_read_is_an_error_event_and_the_rest_applies() {
    let file = vocabulary_file("typo");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "foo bar -> Foobar\nnot an entry\n").unwrap();
    let (daemon, _) = vocal_daemon(foo_bar_phrase(), file);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    let mut error = None;
    loop {
        match watcher.next() {
            Event::Error { message } => error = Some(message),
            event if event == ev_final(1, "Ouvre Foobar.") => break,
            _ => {}
        }
    }
    let error = error.expect("an error event");
    assert!(error.contains("line 2"), "{error}");
}

#[test]
fn last_shows_the_last_phrases_as_they_were_shown() {
    let file = vocabulary_file("last");
    crate::vocabulary::teach(&file, "foo bar", "Foobar").unwrap();
    let (daemon, _) = vocal_daemon(foo_bar_phrase(), file);
    assert_eq!(daemon.last(), Vec::<String>::new());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_final(1, "Ouvre Foobar."));
    assert_eq!(daemon.last(), ["Ouvre Foobar."]);
}

#[test]
fn last_keeps_a_phrase_that_is_not_pasted_and_only_the_latest_ones() {
    let finals = (1..=LAST_PHRASES as u64 + 5)
        .map(|n| (n as usize * 1_600, final_(n, &format!("phrase {n}"))))
        .collect();
    let engine = FakeEngine {
        script: [vec![(0, EngineEvent::Ready)], finals].concat(),
        ..FakeEngine::default()
    };
    let (daemon, _, _) = daemon(FakeMic::default(), engine);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::StartNoPaste).unwrap();
    let total = LAST_PHRASES as u64 + 5;
    watcher.skip_to(&ev_final(total, &format!("phrase {total}")));
    let last = daemon.last();
    assert_eq!(last.len(), LAST_PHRASES);
    assert_eq!(last.first().unwrap(), "phrase 6");
    assert_eq!(last.last().unwrap(), &format!("phrase {total}"));
}

#[test]
fn after_a_restart_last_shows_nothing() {
    let (first, _, _) = daemon(FakeMic::default(), two_closed_phrases());
    let watcher = Watcher::on(&first);
    first.command(Command::Start).unwrap();
    watcher.skip_to(&ev_final(2, "Ça va ?"));
    assert_eq!(first.last(), ["Bonjour.", "Ça va ?"]);

    let (restarted, _, _) = daemon(FakeMic::default(), two_closed_phrases());
    assert_eq!(restarted.last(), Vec::<String>::new());
}

#[test]
fn last_goes_through_the_socket_as_a_list_of_phrases() {
    let path = socket_path("last.sock");
    let (daemon, _, _) = daemon(FakeMic::default(), two_closed_phrases());
    let watcher = Watcher::on(&daemon);
    let listener = bind(&path).unwrap();
    let handle = daemon.clone();
    thread::spawn(move || serve(listener, handle));

    assert_eq!(ask(&path, "last"), "{\"phrases\":[]}\n");
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_final(2, "Ça va ?"));
    assert_eq!(
        ask(&path, "last"),
        "{\"phrases\":[\"Bonjour.\",\"Ça va ?\"]}\n"
    );
}

fn rule(name: &str, class: Option<&str>) -> Rule {
    Rule {
        name: name.into(),
        class: class.map(String::from),
        paste: true,
        enter: false,
        drop_period: false,
        vocabulary: Vec::new(),
    }
}

/// Runs a command in a terminal: the period goes, Enter follows.
fn terminal() -> Rule {
    Rule {
        enter: true,
        drop_period: true,
        ..rule("terminal", Some("foot"))
    }
}

/// An engine that hears `text` as one phrase and nothing else.
fn saying(text: &str) -> FakeEngine {
    FakeEngine {
        script: vec![(0, EngineEvent::Ready)],
        on_finish: vec![final_(1, text)],
        ..FakeEngine::default()
    }
}

/// A daemon with `rules`, pasting into `paste`, reading its common vocabulary from `file`.
fn ruled_daemon(engine: FakeEngine, paste: &FakePaste, rules: Vec<Rule>, file: PathBuf) -> Handle {
    spawn(
        Arc::new(FakeMic::default()),
        Arc::new(engine),
        Some(Box::new(paste.clone())),
        HOLD,
        Arc::new(Instant::now),
        file,
        rules,
    )
}

/// One dictation, begun by `start` and stopped at once, until the daemon is idle again.
fn dictate(daemon: &Handle, start: Command) {
    let watcher = Watcher::on(daemon);
    daemon.command(start).unwrap();
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&state(State::Stopping));
    watcher.skip_to(&state(State::Idle));
}

fn handed(paste: &FakePaste) -> Vec<Handed> {
    paste.0.lock().unwrap().clone()
}

#[test]
fn a_dictated_command_in_a_terminal_is_run_without_its_period() {
    let paste = FakePaste::in_window("foot");
    let daemon = ruled_daemon(saying("ls -la."), &paste, vec![terminal()], no_vocabulary());
    dictate(&daemon, Command::Start);
    assert_eq!(
        handed(&paste),
        [
            Handed::Begin,
            Handed::Phrase("ls -la".into()),
            Handed::Enter
        ]
    );
}

#[test]
fn a_window_that_matches_no_rule_gets_the_default_behaviour() {
    let paste = FakePaste::in_window("org.example.Chat");
    let daemon = ruled_daemon(saying("ls -la."), &paste, vec![terminal()], no_vocabulary());
    dictate(&daemon, Command::Start);
    assert_eq!(
        handed(&paste),
        [Handed::Begin, Handed::Phrase("ls -la.".into())]
    );
}

#[test]
fn the_first_matching_rule_governs_the_dictation() {
    let paste = FakePaste::in_window("foot");
    let rules = vec![
        Rule {
            enter: false,
            ..terminal()
        },
        terminal(),
    ];
    let daemon = ruled_daemon(saying("ls -la."), &paste, rules, no_vocabulary());
    dictate(&daemon, Command::Start);
    assert_eq!(
        handed(&paste),
        [Handed::Begin, Handed::Phrase("ls -la".into())]
    );
}

#[test]
fn enter_alone_sends_a_chat_message_and_keeps_its_period() {
    let paste = FakePaste::in_window("org.example.Chat");
    let chat = Rule {
        enter: true,
        ..rule("chat", Some("org.example.*"))
    };
    let daemon = ruled_daemon(saying("Bonjour."), &paste, vec![chat], no_vocabulary());
    dictate(&daemon, Command::Start);
    assert_eq!(
        handed(&paste),
        [
            Handed::Begin,
            Handed::Phrase("Bonjour.".into()),
            Handed::Enter
        ]
    );
}

#[test]
fn only_the_period_that_ends_the_whole_dictation_is_dropped_and_enter_is_pressed_once() {
    let paste = FakePaste::in_window("foot");
    let engine = FakeEngine {
        script: vec![(0, EngineEvent::Ready), (1_600, final_(1, "cd src."))],
        on_finish: vec![final_(2, "ls.")],
        ..FakeEngine::default()
    };
    let daemon = ruled_daemon(engine, &paste, vec![terminal()], no_vocabulary());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_final(1, "cd src."));
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&state(State::Idle));
    assert_eq!(
        handed(&paste),
        [
            Handed::Begin,
            Handed::Phrase("cd src. ls".into()),
            Handed::Enter
        ]
    );
}

#[test]
fn the_stream_and_last_keep_the_period_the_rule_drops_from_the_paste() {
    let paste = FakePaste::in_window("foot");
    let daemon = ruled_daemon(saying("ls -la."), &paste, vec![terminal()], no_vocabulary());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&ev_final(1, "ls -la."));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(daemon.last(), ["ls -la."]);
}

#[test]
fn a_rule_that_turns_pasting_off_leaves_the_dictation_in_the_stream() {
    let paste = FakePaste::in_window("org.example.Assistant");
    let silent = Rule {
        paste: false,
        enter: true,
        ..rule("assistant", Some("org.example.Assistant"))
    };
    let daemon = ruled_daemon(saying("Allume."), &paste, vec![silent], no_vocabulary());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&ev_final(1, "Allume."));
    watcher.skip_to(&state(State::Idle));
    // Nothing pasted, so no Enter either; the window was only asked its class.
    assert_eq!(handed(&paste), [Handed::Begin]);
    assert_eq!(daemon.last(), ["Allume."]);
}

#[test]
fn a_dictation_with_no_phrase_presses_no_enter() {
    let paste = FakePaste::in_window("foot");
    let daemon = ruled_daemon(ready_engine(), &paste, vec![terminal()], no_vocabulary());
    dictate(&daemon, Command::Start);
    assert_eq!(handed(&paste), [Handed::Begin]);
}

#[test]
fn a_text_that_is_only_a_dropped_period_presses_no_enter() {
    let paste = FakePaste::in_window("foot");
    let daemon = ruled_daemon(saying("."), &paste, vec![terminal()], no_vocabulary());
    dictate(&daemon, Command::Start);
    assert_eq!(handed(&paste), [Handed::Begin]);
}

#[test]
fn the_rule_is_chosen_from_the_window_where_the_dictation_started() {
    let paste = FakePaste::in_window("foot");
    let daemon = ruled_daemon(saying("ls -la."), &paste, vec![terminal()], no_vocabulary());
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    // The focus moves to a chat before the stop: the terminal rule still governs.
    *paste.1.lock().unwrap() = Some("org.example.Chat".into());
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&state(State::Stopping));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(
        handed(&paste),
        [
            Handed::Begin,
            Handed::Phrase("ls -la".into()),
            Handed::Enter
        ]
    );
}

#[test]
fn each_dictation_chooses_its_rule_again() {
    let paste = FakePaste::in_window("foot");
    let daemon = ruled_daemon(saying("ls."), &paste, vec![terminal()], no_vocabulary());
    dictate(&daemon, Command::Start);
    *paste.1.lock().unwrap() = Some("org.example.Chat".into());
    dictate(&daemon, Command::Start);
    assert_eq!(
        handed(&paste),
        [
            Handed::Begin,
            Handed::Phrase("ls".into()),
            Handed::Enter,
            Handed::Begin,
            Handed::Phrase("ls.".into()),
        ]
    );
}

#[test]
fn a_dictation_that_does_not_paste_asks_no_window_and_runs_no_rule() {
    let paste = FakePaste::in_window("foot");
    let daemon = ruled_daemon(saying("ls."), &paste, vec![terminal()], no_vocabulary());
    dictate(&daemon, Command::StartNoPaste);
    assert_eq!(handed(&paste), []);
}

#[test]
fn start_with_a_rule_forces_it_whatever_the_window() {
    let paste = FakePaste::in_window("org.example.Chat");
    let daemon = ruled_daemon(saying("ls -la."), &paste, vec![terminal()], no_vocabulary());
    dictate(&daemon, Command::StartRule("terminal".into()));
    assert_eq!(
        handed(&paste),
        [
            Handed::Begin,
            Handed::Phrase("ls -la".into()),
            Handed::Enter
        ]
    );
}

#[test]
fn a_rule_without_a_class_is_reached_by_its_name_only() {
    let paste = FakePaste::in_window("foot");
    let shell = Rule {
        enter: true,
        ..rule("shell", None)
    };
    let daemon = ruled_daemon(saying("ls."), &paste, vec![shell], no_vocabulary());
    dictate(&daemon, Command::Start);
    assert_eq!(
        handed(&paste),
        [Handed::Begin, Handed::Phrase("ls.".into())]
    );
    dictate(&daemon, Command::StartRule("shell".into()));
    assert_eq!(
        handed(&paste)[2..],
        [Handed::Begin, Handed::Phrase("ls.".into()), Handed::Enter]
    );
}

#[test]
fn a_forced_rule_governs_one_dictation_only() {
    let paste = FakePaste::in_window("org.example.Chat");
    let daemon = ruled_daemon(saying("ls."), &paste, vec![terminal()], no_vocabulary());
    dictate(&daemon, Command::StartRule("terminal".into()));
    dictate(&daemon, Command::Start);
    assert_eq!(
        handed(&paste)[3..],
        [Handed::Begin, Handed::Phrase("ls.".into())]
    );
}

#[test]
fn a_key_pressed_with_a_rule_forces_it_for_the_dictation_it_starts() {
    let clock = FakeClock::new();
    let paste = FakePaste::in_window("org.example.Chat");
    let daemon = spawn(
        Arc::new(FakeMic::default()),
        Arc::new(saying("ls -la.")),
        Some(Box::new(paste.clone())),
        HOLD,
        clock.clock(),
        no_vocabulary(),
        vec![terminal()],
    );
    let watcher = Watcher::on(&daemon);
    assert_eq!(
        daemon.command(Command::PressRule("terminal".into())),
        Ok(State::Loading)
    );
    clock.advance(ms(500));
    assert_eq!(daemon.command(Command::Release), Ok(State::Stopping));
    watcher.skip_to(&state(State::Stopping));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(
        handed(&paste),
        [
            Handed::Begin,
            Handed::Phrase("ls -la".into()),
            Handed::Enter
        ]
    );
}

#[test]
fn an_unknown_rule_refuses_the_start_before_the_microphone_opens() {
    let mic = Arc::new(FakeMic::default());
    let paste = FakePaste::in_window("foot");
    let daemon = spawn(
        mic.clone(),
        Arc::new(ready_engine()),
        Some(Box::new(paste.clone())),
        HOLD,
        Arc::new(Instant::now),
        no_vocabulary(),
        vec![terminal()],
    );
    let watcher = Watcher::on(&daemon);
    for command in [
        Command::StartRule("nope".into()),
        Command::PressRule("nope".into()),
    ] {
        let error = daemon.command(command).unwrap_err();
        assert!(error.contains("nope"), "{error}");
    }
    assert_eq!(daemon.command(Command::Status), Ok(State::Idle));
    assert_eq!(mic.opened.load(Ordering::SeqCst), 0);
    assert_eq!(handed(&paste), []);
    watcher.skip_to(&Event::Error {
        message: "no rule is named nope".into(),
    });
}

/// A common vocabulary of one entry, in its own file.
fn common_vocabulary(name: &str, line: &str) -> PathBuf {
    let file = vocabulary_file(name);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, format!("{line}\n")).unwrap();
    file
}

#[test]
fn a_rule_adds_its_vocabulary_to_the_common_one_for_its_dictations_only() {
    let file = common_vocabulary("rule-adds", "foo bar -> Foobar");
    let paste = FakePaste::in_window("foot");
    let own = Rule {
        vocabulary: vec!["baz -> Baz".into()],
        ..terminal()
    };
    let engine = FakeEngine {
        script: vec![(0, EngineEvent::Ready), (1_600, partial(1, "foo bar baz"))],
        on_finish: vec![final_(1, "foo bar baz.")],
        ..FakeEngine::default()
    };
    let daemon = ruled_daemon(engine, &paste, vec![own], file);
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_partial(1, "Foobar Baz"));
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&ev_final(1, "Foobar Baz."));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(handed(&paste)[1], Handed::Phrase("Foobar Baz".into()));

    // A window with no rule: the common vocabulary alone.
    *paste.1.lock().unwrap() = Some("org.example.Chat".into());
    daemon.command(Command::Start).unwrap();
    daemon.command(Command::Stop).unwrap();
    watcher.skip_to(&ev_final(2, "Foobar baz."));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(handed(&paste)[4], Handed::Phrase("Foobar baz.".into()));
}

#[test]
fn on_the_same_heard_words_the_rule_vocabulary_wins_over_the_common_one() {
    let file = common_vocabulary("rule-wins", "foo bar -> Foobar");
    let paste = FakePaste::in_window("foot");
    let own = Rule {
        vocabulary: vec!["foo bar -> foo_bar".into()],
        ..terminal()
    };
    let daemon = ruled_daemon(saying("ouvre foo bar."), &paste, vec![own], file);
    dictate(&daemon, Command::Start);
    assert_eq!(handed(&paste)[1], Handed::Phrase("ouvre foo_bar".into()));
}

#[test]
fn a_rule_is_forced_through_the_socket_and_an_unknown_one_is_told() {
    let path = socket_path("rule.sock");
    let paste = FakePaste::in_window("org.example.Chat");
    let daemon = ruled_daemon(saying("ls."), &paste, vec![terminal()], no_vocabulary());
    let listener = bind(&path).unwrap();
    thread::spawn(move || serve(listener, daemon));

    assert_eq!(
        ask(&path, "start --rule nope"),
        "{\"error\":\"no rule is named nope\"}\n"
    );
    assert_eq!(
        ask(&path, "start --rule terminal"),
        "{\"state\":\"loading\"}\n"
    );
    assert_eq!(ask(&path, "stop"), "{\"state\":\"stopping\"}\n");
    wait_until("the paste", || handed(&paste).len() == 3);
    assert_eq!(
        handed(&paste),
        [Handed::Begin, Handed::Phrase("ls".into()), Handed::Enter]
    );
}

#[test]
fn a_rule_name_with_a_space_or_nothing_is_not_a_command() {
    for word in [
        "start --rule",
        "start --rule ",
        "start --rule a b",
        "press --rule  a",
    ] {
        assert_eq!(Command::parse(word), None, "{word:?}");
    }
}

use crate::stats::{Dictation, Store, UNKNOWN};

/// What the daemon kept of its dictations; with `fail`, a disk that refuses.
#[derive(Clone, Default)]
struct FakeLedger {
    kept: Arc<Mutex<Vec<Dictation>>>,
    fail: bool,
}

impl crate::stats::Ledger for FakeLedger {
    fn record(&self, dictation: &Dictation) -> io::Result<()> {
        if self.fail {
            return Err(io::Error::other("disk full"));
        }
        self.kept.lock().unwrap().push(dictation.clone());
        Ok(())
    }
}

impl FakeLedger {
    fn kept(&self) -> Vec<Dictation> {
        self.kept.lock().unwrap().clone()
    }
}

/// A daemon that keeps the statistics of its dictations in `ledger`.
fn recording_daemon(
    ledger: Box<dyn crate::stats::Ledger>,
    mic: Arc<dyn Microphone>,
    engine: Arc<dyn Engine>,
    paste: Option<&FakePaste>,
    clock: Clock,
    rules: Vec<Rule>,
) -> Handle {
    spawn_recording(
        Some(ledger),
        mic,
        engine,
        paste.map(|paste| Box::new(paste.clone()) as Box<dyn Paste>),
        HOLD,
        clock,
        no_vocabulary(),
        rules,
    )
}

/// A daemon over an invented session: 12 seconds of audio, the engine saying `text`.
fn session(ledger: &FakeLedger, paste: Option<&FakePaste>, text: &str) -> Handle {
    let mic = FakeMic {
        chunks: Some(120),
        ..FakeMic::default()
    };
    recording_daemon(
        Box::new(ledger.clone()),
        Arc::new(mic),
        Arc::new(saying(text)),
        paste,
        Arc::new(Instant::now),
        Vec::new(),
    )
}

/// A dictation that ends by itself, when the microphone has nothing more to give.
fn dictate_to_the_end(daemon: &Handle, start: Command) {
    let watcher = Watcher::on(daemon);
    daemon.command(start).unwrap();
    watcher.skip_to(&state(State::Stopping));
    watcher.skip_to(&state(State::Idle));
}

#[test]
fn a_dictation_is_kept_with_its_application_its_words_and_its_audio() {
    let paste = FakePaste::in_window("org.example.Editor");
    let ledger = FakeLedger::default();
    let daemon = session(&ledger, Some(&paste), "Bonjour tout le monde ?");
    dictate_to_the_end(&daemon, Command::Start);

    let kept = ledger.kept();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0].app, "org.example.Editor");
    // The lone question mark is not a word.
    assert_eq!(kept[0].words, 4);
    assert_eq!(kept[0].audio_ms, 12_000);
    assert!(kept[0].paste_ms.is_some(), "{kept:?}");
}

#[test]
fn the_time_to_paste_runs_from_the_stop_to_the_paste() {
    let clock = FakeClock::new();
    let engine = Arc::new(FakeEngine {
        script: vec![
            (0, EngineEvent::Ready),
            (1_600, final_(1, "Bonjour tout le monde.")),
        ],
        hold_end: true,
        ..FakeEngine::default()
    });
    let paste = FakePaste::in_window("org.example.Editor");
    let ledger = FakeLedger::default();
    let daemon = recording_daemon(
        Box::new(ledger.clone()),
        Arc::new(FakeMic::default()),
        engine.clone(),
        Some(&paste),
        clock.clock(),
        Vec::new(),
    );
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    watcher.skip_to(&ev_final(1, "Bonjour tout le monde."));

    // The time spent speaking is not the time to paste.
    clock.advance(Duration::from_secs(5));
    daemon.command(Command::Stop).unwrap();
    clock.advance(Duration::from_millis(320));
    engine.emit(EngineEvent::Flushed);
    wait_until("the dictation is kept", || !ledger.kept().is_empty());
    assert_eq!(ledger.kept()[0].paste_ms, Some(320));

    // The engine ending after its flush does not keep the dictation twice.
    engine.emit(EngineEvent::Ended(Ok(())));
    watcher.skip_to(&state(State::Idle));
    assert_eq!(ledger.kept().len(), 1);
}

#[test]
fn a_dictation_with_nothing_pasted_is_kept_under_an_unknown_application() {
    let ledger = FakeLedger::default();

    // `start --no-paste`: the window is not even asked.
    let paste = FakePaste::in_window("org.example.Editor");
    let daemon = session(&ledger, Some(&paste), "Ça va bien ?");
    dictate_to_the_end(&daemon, Command::StartNoPaste);
    assert_eq!(handed(&paste), []);

    // `paste = false` in the configuration: there is no output at all.
    let daemon = session(&ledger, None, "Ça va bien ?");
    dictate_to_the_end(&daemon, Command::Start);

    let kept = ledger.kept();
    assert_eq!(kept.len(), 2, "{kept:?}");
    for dictation in kept {
        assert_eq!(dictation.app, UNKNOWN);
        assert_eq!(dictation.words, 3);
        assert_eq!(dictation.paste_ms, None);
    }
}

#[test]
fn a_rule_that_pastes_nothing_keeps_the_application_of_the_window() {
    let ledger = FakeLedger::default();
    let paste = FakePaste::in_window("org.example.Chat");
    let quiet = Rule {
        paste: false,
        ..rule("quiet", Some("org.example.Chat"))
    };
    let daemon = recording_daemon(
        Box::new(ledger.clone()),
        Arc::new(FakeMic::default()),
        Arc::new(saying("Ça va bien ?")),
        Some(&paste),
        Arc::new(Instant::now),
        vec![quiet],
    );
    dictate(&daemon, Command::Start);
    let kept = ledger.kept();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0].app, "org.example.Chat");
    assert_eq!(kept[0].words, 3);
    assert_eq!(kept[0].paste_ms, None);
}

#[test]
fn a_window_the_desktop_could_not_name_is_an_unknown_application() {
    let ledger = FakeLedger::default();
    let paste = FakePaste::default();
    let daemon = session(&ledger, Some(&paste), "Ça va bien ?");
    dictate_to_the_end(&daemon, Command::Start);
    let kept = ledger.kept();
    assert_eq!(kept[0].app, UNKNOWN);
    assert!(kept[0].paste_ms.is_some());
}

#[test]
fn a_dictation_that_heard_nothing_is_not_kept() {
    let ledger = FakeLedger::default();
    let paste = FakePaste::in_window("org.example.Editor");
    let daemon = recording_daemon(
        Box::new(ledger.clone()),
        Arc::new(FakeMic::default()),
        Arc::new(ready_engine()),
        Some(&paste),
        Arc::new(Instant::now),
        Vec::new(),
    );
    dictate(&daemon, Command::Start);
    assert_eq!(ledger.kept(), []);
}

#[test]
fn each_dictation_is_kept_once_whatever_the_engine_says_at_its_end() {
    let ledger = FakeLedger::default();
    let engine = FakeEngine {
        script: vec![(0, EngineEvent::Ready)],
        on_finish: vec![final_(1, "Bonjour."), EngineEvent::Flushed],
        ..FakeEngine::default()
    };
    let daemon = recording_daemon(
        Box::new(ledger.clone()),
        Arc::new(FakeMic::default()),
        Arc::new(engine),
        Some(&FakePaste::in_window("org.example.Editor")),
        Arc::new(Instant::now),
        Vec::new(),
    );
    dictate(&daemon, Command::Start);
    dictate(&daemon, Command::Start);
    assert_eq!(ledger.kept().len(), 2);
}

#[test]
fn a_store_that_cannot_be_written_is_an_error_event_and_the_dictation_still_pastes() {
    let ledger = FakeLedger {
        fail: true,
        ..FakeLedger::default()
    };
    let paste = FakePaste::in_window("org.example.Editor");
    let daemon = recording_daemon(
        Box::new(ledger),
        Arc::new(FakeMic::default()),
        Arc::new(saying("Quarante licornes dansent.")),
        Some(&paste),
        Arc::new(Instant::now),
        Vec::new(),
    );
    let watcher = Watcher::on(&daemon);
    daemon.command(Command::Start).unwrap();
    daemon.command(Command::Stop).unwrap();
    // What the event says holds no text: the dictated words stay out of it.
    watcher.skip_to(&Event::Error {
        message: "the statistics could not be written: disk full".into(),
    });
    watcher.skip_to(&state(State::Idle));
    assert_eq!(
        handed(&paste),
        [
            Handed::Begin,
            Handed::Phrase("Quarante licornes dansent.".into())
        ]
    );
}

/// A folder of its own for a store, outside the repository.
fn stats_folder(name: &str) -> PathBuf {
    let folder = std::env::temp_dir().join(format!("vocaline-stats-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    folder
}

#[test]
fn the_store_holds_no_text_after_a_scripted_session() {
    let folder = stats_folder("no-text");
    let store = folder.join("vocaline").join("stats.jsonl");
    let phrases = ["Quarante licornes dansent", "sous Jupiter"];
    let engine = FakeEngine {
        script: vec![
            (0, EngineEvent::Ready),
            (1_600, partial(1, phrases[0])),
            (3_200, final_(1, &format!("{}.", phrases[0]))),
            (4_800, partial(2, phrases[1])),
        ],
        on_finish: vec![final_(2, phrases[1])],
        ..FakeEngine::default()
    };
    let paste = FakePaste::in_window("org.example.Editor");
    let daemon = recording_daemon(
        Box::new(Store::new(store.clone())),
        Arc::new(FakeMic::default()),
        Arc::new(engine),
        Some(&paste),
        Arc::new(Instant::now),
        Vec::new(),
    );
    dictate(&daemon, Command::Start);
    dictate(&daemon, Command::StartNoPaste);

    // Everything the daemon left in the folder, whatever its name.
    let mut written = String::new();
    for file in std::fs::read_dir(store.parent().unwrap()).unwrap() {
        written += &std::fs::read_to_string(file.unwrap().path()).unwrap();
    }
    assert_eq!(written.lines().count(), 2, "{written}");
    let heard = phrases.join(" ").to_lowercase();
    for word in heard.split_whitespace() {
        assert!(!written.to_lowercase().contains(word), "{word}: {written}");
    }
    // Only counts and durations, and the class of the window: no other key, no other text.
    for line in written.lines() {
        let line: serde_json::Value = serde_json::from_str(line).unwrap();
        for (key, value) in line.as_object().unwrap() {
            match key.as_str() {
                "day" | "app" => assert!(value.is_string(), "{key}"),
                "words" | "audio_ms" | "paste_ms" => assert!(value.is_u64(), "{key}"),
                _ => panic!("{key} is not a count or a duration"),
            }
        }
    }
    assert_eq!(written.matches("org.example.Editor").count(), 1);
}

#[test]
fn a_few_dictations_in_two_applications_show_up_per_application() {
    let folder = stats_folder("two-apps");
    let store = Store::new(folder.join("stats.jsonl"));
    let paste = FakePaste::in_window("org.example.Editor");
    let mic = || {
        Arc::new(FakeMic {
            chunks: Some(120),
            ..FakeMic::default()
        })
    };
    let daemon = recording_daemon(
        Box::new(Store::new(folder.join("stats.jsonl"))),
        mic(),
        Arc::new(saying("Bonjour tout le monde.")),
        Some(&paste),
        Arc::new(Instant::now),
        Vec::new(),
    );
    dictate_to_the_end(&daemon, Command::Start);
    dictate_to_the_end(&daemon, Command::Start);
    *paste.1.lock().unwrap() = Some("org.example.Chat".into());
    dictate_to_the_end(&daemon, Command::Start);

    let mut out = Vec::new();
    crate::stats::print(&store, jiff::Zoned::now().date(), false, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    let line = |app: &str| {
        text.lines()
            .find(|line| line.contains(app))
            .unwrap_or_else(|| panic!("{app}: {text}"))
            .to_string()
    };
    // Three of twelve seconds, four words each.
    assert!(text.contains("3 dictations, 12 words, 0.6 min"), "{text}");
    assert!(line("org.example.Editor").contains("2 dictations, 8 words, 0.4 min"));
    assert!(line("org.example.Chat").contains("1 dictation, 4 words, 0.2 min"));
}
