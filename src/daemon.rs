//! The daemon: owns the microphone and the engine, takes commands, publishes the event stream.
//!
//! One thread holds the state and receives everything that changes it (commands, watchers,
//! engine events), so the state needs no lock. A dictation adds two threads: the pump reads
//! the microphone and publishes its level, the feeder writes the audio to the engine. Between
//! them an unbounded queue: the microphone is never held back by an engine still loading.

use crate::audio::{Capture, Microphone, SAMPLE_RATE, rms};
use crate::engine::{Engine, EngineEvent, EngineInput};
use crate::events::{Event, State, VERSION};
use crate::steady::Steady;
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// A watcher that cannot take a line within this delay is dropped rather than slowing the
/// daemon down.
const WATCHER_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// The key went down: stop when listening, otherwise start and remember when.
    Press,
    /// The key went up: stop if it was held long enough.
    Release,
    Start,
    Stop,
    Toggle,
    Status,
}

impl Command {
    pub fn parse(word: &str) -> Option<Command> {
        match word {
            "press" => Some(Command::Press),
            "release" => Some(Command::Release),
            "start" => Some(Command::Start),
            "stop" => Some(Command::Stop),
            "toggle" => Some(Command::Toggle),
            "status" => Some(Command::Status),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Command::Press => "press",
            Command::Release => "release",
            Command::Start => "start",
            Command::Stop => "stop",
            Command::Toggle => "toggle",
            Command::Status => "status",
        }
    }
}

/// The state reached, or why the command was refused.
pub type Reply = Result<State, String>;

/// The answer line of the control socket (see docs/events.md).
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<State>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl From<Reply> for Answer {
    fn from(reply: Reply) -> Answer {
        match reply {
            Ok(state) => Answer {
                state: Some(state),
                error: None,
            },
            Err(error) => Answer {
                state: None,
                error: Some(error),
            },
        }
    }
}

enum Message {
    Command(Command, mpsc::Sender<Reply>),
    Watch(Box<dyn Write + Send>),
    /// From the dictation numbered by the first field.
    Engine(u64, EngineEvent),
    CaptureEnded(u64, Option<String>),
}

#[derive(Clone)]
pub struct Handle {
    messages: mpsc::Sender<Message>,
}

impl Handle {
    pub fn command(&self, command: Command) -> Reply {
        let (reply, answer) = mpsc::channel();
        self.messages
            .send(Message::Command(command, reply))
            .map_err(|_| "the daemon has stopped".to_string())?;
        answer
            .recv()
            .map_err(|_| "the daemon has stopped".to_string())?
    }

    /// Sends the event stream to `watcher`, from a hello and the current state on.
    pub fn watch(&self, watcher: Box<dyn Write + Send>) {
        let _ = self.messages.send(Message::Watch(watcher));
    }
}

/// What tells the time to the key's hold-or-tap rule; a test moves it by hand.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Starts the daemon's thread. A key held for `hold` or more stops the dictation it started when
/// it is released.
pub fn spawn(
    mic: Arc<dyn Microphone>,
    engine: Arc<dyn Engine>,
    hold: Duration,
    clock: Clock,
) -> Handle {
    let (messages, inbox) = mpsc::channel();
    let mut core = Core {
        mic,
        engine,
        messages: messages.clone(),
        bus: Arc::new(Bus::default()),
        state: State::Idle,
        dictation: 0,
        stop: Arc::new(AtomicBool::new(false)),
        last_utterance: 0,
        open_utterance: None,
        partial: Steady::default(),
        hold,
        clock,
        pressed_at: None,
    };
    thread::spawn(move || {
        for message in inbox {
            core.handle(message);
        }
    });
    Handle { messages }
}

struct Core {
    mic: Arc<dyn Microphone>,
    engine: Arc<dyn Engine>,
    messages: mpsc::Sender<Message>,
    bus: Arc<Bus>,
    state: State,
    /// Numbers the dictations, so a late event of an ended one is not taken for the current.
    dictation: u64,
    /// Tells the pump of the current dictation to close the microphone.
    stop: Arc<AtomicBool>,
    last_utterance: u64,
    /// The engine's number of the open utterance, and ours.
    open_utterance: Option<(u64, u64)>,
    /// The partial of the open utterance, as shown.
    partial: Steady,
    hold: Duration,
    clock: Clock,
    /// When the key went down and started the dictation; none for a press that stopped it.
    pressed_at: Option<Instant>,
}

impl Core {
    fn handle(&mut self, message: Message) {
        match message {
            Message::Command(command, reply) => {
                let _ = reply.send(self.command(command));
            }
            Message::Watch(watcher) => self.bus.add(
                watcher,
                &[
                    Event::Hello { version: VERSION },
                    Event::State { state: self.state },
                ],
            ),
            Message::Engine(dictation, event) if dictation == self.dictation => {
                self.engine_event(event)
            }
            Message::CaptureEnded(dictation, error) if dictation == self.dictation => {
                if let Some(message) = error {
                    self.bus.publish(&Event::Error { message });
                }
                if matches!(self.state, State::Loading | State::Listening) {
                    self.set(State::Stopping);
                }
            }
            Message::Engine(..) | Message::CaptureEnded(..) => {}
        }
    }

    fn command(&mut self, command: Command) -> Reply {
        match (command, self.state) {
            (Command::Status, state) => Ok(state),
            (Command::Press, State::Idle) => {
                let reply = self.start();
                if reply.is_ok() {
                    self.pressed_at = Some((self.clock)());
                }
                reply
            }
            (Command::Press, State::Stopping) => {
                self.pressed_at = None;
                Err("still stopping the last dictation, try again".into())
            }
            (Command::Press, State::Loading | State::Listening) => {
                self.pressed_at = None;
                self.command(Command::Stop)
            }
            // A release with no press before it (the key was down before the daemon ran, or the
            // press was lost) has no start to answer for: it is not a hold.
            (Command::Release, state) => match self.pressed_at.take() {
                Some(at)
                    if matches!(state, State::Loading | State::Listening)
                        && (self.clock)().saturating_duration_since(at) >= self.hold =>
                {
                    self.command(Command::Stop)
                }
                _ => Ok(state),
            },
            (Command::Start | Command::Toggle, State::Idle) => self.start(),
            (Command::Start, state @ (State::Loading | State::Listening)) => Ok(state),
            (Command::Start | Command::Toggle, State::Stopping) => {
                Err("still stopping the last dictation, try again".into())
            }
            (Command::Stop | Command::Toggle, State::Loading | State::Listening) => {
                self.stop.store(true, Ordering::SeqCst);
                self.set(State::Stopping);
                Ok(State::Stopping)
            }
            (Command::Stop, state @ (State::Idle | State::Stopping)) => Ok(state),
        }
    }

    fn start(&mut self) -> Reply {
        // The microphone first: if it cannot open, no model is loaded for nothing.
        let capture = self
            .mic
            .open()
            .map_err(|e| self.failed(format!("the microphone could not be opened: {e}")))?;
        self.dictation += 1;
        let dictation = self.dictation;
        let messages = self.messages.clone();
        let input = self
            .engine
            .open(Box::new(move |event| {
                let _ = messages.send(Message::Engine(dictation, event));
            }))
            .map_err(|e| self.failed(format!("the engine could not start: {e}")))?;

        self.stop = Arc::new(AtomicBool::new(false));
        self.open_utterance = None;
        let (audio, queue) = mpsc::channel();
        thread::spawn(move || feed(input, queue));
        let pump = Pump {
            capture,
            stop: Arc::clone(&self.stop),
            bus: Arc::clone(&self.bus),
            audio,
            messages: self.messages.clone(),
            dictation,
        };
        thread::spawn(move || pump.run());
        self.set(State::Loading);
        Ok(State::Loading)
    }

    fn failed(&self, message: String) -> String {
        self.bus.publish(&Event::Error {
            message: message.clone(),
        });
        message
    }

    fn engine_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Ready => {
                if self.state == State::Loading {
                    self.set(State::Listening);
                }
            }
            EngineEvent::Partial { utterance, text } => {
                let utterance = self.utterance(utterance);
                if let Some(text) = self.partial.read(&text) {
                    self.bus.publish(&Event::Partial { utterance, text });
                }
            }
            EngineEvent::Final { utterance, text } => {
                let utterance = self.utterance(utterance);
                self.open_utterance = None;
                self.bus.publish(&Event::Final { utterance, text });
            }
            EngineEvent::Silence => self.bus.publish(&Event::Silence),
            EngineEvent::Ended(result) => {
                if let Err(message) = result {
                    self.bus.publish(&Event::Error { message });
                }
                // The engine may have failed while the microphone is still open.
                self.stop.store(true, Ordering::SeqCst);
                self.set(State::Idle);
            }
        }
    }

    /// Our number for the engine's utterance: it only grows, across dictations.
    fn utterance(&mut self, engine: u64) -> u64 {
        match self.open_utterance {
            Some((open, ours)) if open == engine => ours,
            _ => {
                self.last_utterance += 1;
                self.open_utterance = Some((engine, self.last_utterance));
                self.partial = Steady::default();
                self.last_utterance
            }
        }
    }

    fn set(&mut self, state: State) {
        if self.state != state {
            self.state = state;
            self.bus.publish(&Event::State { state });
        }
    }
}

/// Reads the microphone until told to stop or until it ends.
struct Pump {
    capture: Box<dyn Capture>,
    stop: Arc<AtomicBool>,
    bus: Arc<Bus>,
    audio: mpsc::Sender<Vec<i16>>,
    messages: mpsc::Sender<Message>,
    dictation: u64,
}

impl Pump {
    fn run(mut self) {
        // 100 ms: the cadence of the level, and well under the engine's step.
        let mut buf = vec![0; SAMPLE_RATE as usize / 10];
        let mut error = None;
        while !self.stop.load(Ordering::SeqCst) {
            match self.capture.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    self.bus.publish(&Event::level(rms(&buf[..n])));
                    if self.audio.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    error = Some(format!("the microphone failed: {e}"));
                    break;
                }
            }
        }
        // Told before the queue closes, so the daemon is stopping before the engine has ended.
        if !self.stop.load(Ordering::SeqCst) {
            let _ = self
                .messages
                .send(Message::CaptureEnded(self.dictation, error));
        }
        // Closing the queue lets the feeder finish the engine's stream.
    }
}

/// Writes the queued audio to the engine, then ends its stream, which finalises the open phrase.
fn feed(mut input: Box<dyn EngineInput>, queue: mpsc::Receiver<Vec<i16>>) {
    for chunk in queue {
        // The engine has gone: it reports why through its own events.
        if input.write(&chunk).is_err() {
            return;
        }
    }
    input.finish();
}

/// The watchers of the event stream.
#[derive(Default)]
struct Bus {
    watchers: Mutex<Vec<Box<dyn Write + Send>>>,
}

impl Bus {
    fn add(&self, mut watcher: Box<dyn Write + Send>, first: &[Event]) {
        let mut watchers = self.watchers.lock().unwrap();
        if first.iter().all(|event| send(&mut watcher, event)) {
            watchers.push(watcher);
        }
    }

    fn publish(&self, event: &Event) {
        self.watchers
            .lock()
            .unwrap()
            .retain_mut(|watcher| send(watcher, event));
    }
}

fn send(watcher: &mut Box<dyn Write + Send>, event: &Event) -> bool {
    let line = event.to_line() + "\n";
    watcher
        .write_all(line.as_bytes())
        .and_then(|()| watcher.flush())
        .is_ok()
}

/// Binds the control socket, replacing one left by a daemon that died.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    if UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("a daemon already listens on {}", path.display()),
        ));
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    UnixListener::bind(path)
}

/// Answers the clients of the control socket, one thread each.
pub fn serve(listener: UnixListener, daemon: Handle) {
    for stream in listener.incoming().flatten() {
        let daemon = daemon.clone();
        thread::spawn(move || client(stream, daemon));
    }
}

fn client(stream: UnixStream, daemon: Handle) {
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_err() {
        return;
    }
    let word = line.trim();
    if word == "watch" {
        if stream.set_write_timeout(Some(WATCHER_TIMEOUT)).is_ok() {
            daemon.watch(Box::new(stream));
        }
        return;
    }
    let reply = match Command::parse(word) {
        Some(command) => daemon.command(command),
        None => Err(format!("unknown command: {word}")),
    };
    let answer = serde_json::to_string(&Answer::from(reply)).expect("an answer serialises");
    let _ = (&stream).write_all(format!("{answer}\n").as_bytes());
}

#[cfg(test)]
mod tests;
