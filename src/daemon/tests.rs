use super::*;
use crate::audio::Capture;
use crate::engine::{EngineInput, Sink};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(5);

/// A microphone that hands over 100 ms of a steady tone every 5 ms, `chunks` times (forever
/// when `None`), and counts the captures left open.
#[derive(Default)]
struct FakeMic {
    chunks: Option<usize>,
    fail: bool,
    opened: AtomicUsize,
    open_now: Arc<AtomicUsize>,
}

struct FakeCapture {
    left: Option<usize>,
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
            open_now: Arc::clone(&self.open_now),
        }))
    }
}

impl Capture for FakeCapture {
    fn read(&mut self, buf: &mut [i16]) -> io::Result<usize> {
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
/// `on_finish` when the audio ends, then `Ended` unless `hold_end`.
#[derive(Default)]
struct FakeEngine {
    script: Vec<(usize, EngineEvent)>,
    on_finish: Vec<EngineEvent>,
    hold_end: bool,
    fail: bool,
    sink: Mutex<Option<Arc<Sink>>>,
}

struct FakeInput {
    script: VecDeque<(usize, EngineEvent)>,
    on_finish: Vec<EngineEvent>,
    hold_end: bool,
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
        let mut input = FakeInput {
            script: self.script.clone().into(),
            on_finish: self.on_finish.clone(),
            hold_end: self.hold_end,
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
        if !self.hold_end {
            (self.sink)(EngineEvent::Ended(Ok(())));
        }
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
    let handle = spawn(mic.clone(), engine.clone());
    (handle, mic, engine)
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
