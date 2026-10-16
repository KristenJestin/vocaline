//! The daemon: owns the microphone and the engine, takes commands, publishes the event stream.
//!
//! One thread holds the state and receives everything that changes it (commands, watchers,
//! engine events), so the state needs no lock. A dictation adds two threads: the pump reads
//! the microphone and publishes its level, the feeder writes the audio to the engine. Between
//! them an unbounded queue: the microphone is never held back by an engine still loading.

use crate::audio::{Capture, Microphone, SAMPLE_RATE, rms};
use crate::engine::{Engine, EngineEvent, EngineInput, Failure};
use crate::events::{Event, State, VERSION};
use crate::media::Pauser;
use crate::output::Paste;
use crate::phrases::{self, Fillers};
use crate::rules::{self, Rule};
use crate::stats::{self, Dictation, Ledger};
use crate::steady::Steady;
use crate::vocabulary::Vocabulary;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// A watcher that cannot take a line within this delay is dropped rather than slowing the
/// daemon down.
const WATCHER_TIMEOUT: Duration = Duration::from_secs(1);

/// How many final phrases `last` can show.
const LAST_PHRASES: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// The key went down: stop when listening, otherwise start and remember when.
    Press,
    /// `Press`, and if it starts a dictation, the rule of that name governs it.
    PressRule(String),
    /// The key went up: stop if it was held long enough.
    Release,
    Start,
    /// Start, with the rule of that name governing the dictation whatever the window.
    StartRule(String),
    /// Start, with nothing pasted: for a program that only reads the stream.
    StartNoPaste,
    Stop,
    Toggle,
    Status,
}

impl Command {
    /// The rule the command forces.
    fn rule(&self) -> Option<&str> {
        match self {
            Command::PressRule(name) | Command::StartRule(name) => Some(name),
            _ => None,
        }
    }

    pub fn parse(word: &str) -> Option<Command> {
        let named = |prefix: &str| {
            word.strip_prefix(prefix)
                .filter(|name| !name.is_empty() && !name.contains(char::is_whitespace))
                .map(String::from)
        };
        if let Some(name) = named("start --rule ") {
            return Some(Command::StartRule(name));
        }
        if let Some(name) = named("press --rule ") {
            return Some(Command::PressRule(name));
        }
        match word {
            "press" => Some(Command::Press),
            "release" => Some(Command::Release),
            "start" => Some(Command::Start),
            "start --no-paste" => Some(Command::StartNoPaste),
            "stop" => Some(Command::Stop),
            "toggle" => Some(Command::Toggle),
            "status" => Some(Command::Status),
            _ => None,
        }
    }

    pub fn word(&self) -> String {
        match self {
            Command::Press => "press".into(),
            Command::PressRule(name) => format!("press --rule {name}"),
            Command::Release => "release".into(),
            Command::Start => "start".into(),
            Command::StartRule(name) => format!("start --rule {name}"),
            Command::StartNoPaste => "start --no-paste".into(),
            Command::Stop => "stop".into(),
            Command::Toggle => "toggle".into(),
            Command::Status => "status".into(),
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
    /// The answer to `last`: the final phrases, oldest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phrases: Option<Vec<String>>,
}

impl From<Reply> for Answer {
    fn from(reply: Reply) -> Answer {
        match reply {
            Ok(state) => Answer {
                state: Some(state),
                error: None,
                phrases: None,
            },
            Err(error) => Answer {
                state: None,
                error: Some(error),
                phrases: None,
            },
        }
    }
}

enum Message {
    Command(Command, mpsc::Sender<Reply>),
    Watch(Box<dyn Write + Send>),
    Last(mpsc::Sender<Vec<String>>),
    /// From the dictation numbered by the first field.
    Engine(u64, EngineEvent),
    CaptureEnded(u64, Option<String>),
    /// All the audio of the dictation, once the microphone has closed.
    Heard(u64, Vec<i16>),
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

    /// The last final phrases of this session, oldest first.
    pub fn last(&self) -> Vec<String> {
        let (reply, answer) = mpsc::channel();
        let _ = self.messages.send(Message::Last(reply));
        answer.recv().unwrap_or_default()
    }

    /// Sends the event stream to `watcher`, from a hello and the current state on.
    pub fn watch(&self, watcher: Box<dyn Write + Send>) {
        let _ = self.messages.send(Message::Watch(watcher));
    }
}

/// What tells the time to the key's hold-or-tap rule; a test moves it by hand.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// `spawn_recording` with nothing kept of the dictations: most tests have no use for it.
#[cfg(test)]
pub fn spawn(
    mic: Arc<dyn Microphone>,
    engine: Arc<dyn Engine>,
    paste: Option<Box<dyn Paste>>,
    hold: Duration,
    clock: Clock,
    vocabulary_file: PathBuf,
    rules: Vec<Rule>,
) -> Handle {
    spawn_recording(
        None,
        mic,
        engine,
        paste,
        hold,
        clock,
        vocabulary_file,
        Fillers::default(),
        rules,
        None,
    )
}

/// Starts the daemon's thread. The finals of a dictation go to `paste`, if any, in one piece
/// once it has stopped. A key held for `hold` or more stops the dictation it started when it
/// is released. The vocabulary file is read again at each start, so a `teach` or an edit by
/// hand applies to the next dictation. The `fillers` are dropped from every phrase, after the
/// vocabulary has corrected it. The first of `rules` that matches the window the
/// dictation starts in governs it. What is kept of each dictation (counts and durations,
/// never its text) goes to `ledger`, if any. The media players `media` is given are paused
/// for the whole of each dictation.
#[allow(clippy::too_many_arguments)]
pub fn spawn_recording(
    ledger: Option<Box<dyn Ledger>>,
    mic: Arc<dyn Microphone>,
    engine: Arc<dyn Engine>,
    paste: Option<Box<dyn Paste>>,
    hold: Duration,
    clock: Clock,
    vocabulary_file: PathBuf,
    fillers: Fillers,
    rules: Vec<Rule>,
    media: Option<Pauser>,
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
        paste,
        pasting: false,
        dictated: Vec::new(),
        hold,
        clock,
        pressed_at: None,
        vocabulary_file,
        vocabulary: Vocabulary::default(),
        fillers,
        rules,
        rule: None,
        last: VecDeque::new(),
        shown: 0,
        heard: None,
        cpu: None,
        ledger,
        tally: None,
        stopped_at: None,
        media,
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
    paste: Option<Box<dyn Paste>>,
    /// The current dictation's finals are pasted.
    pasting: bool,
    /// The finals of the current dictation, kept for the single paste that follows its end.
    dictated: Vec<String>,
    hold: Duration,
    clock: Clock,
    /// When the key went down and started the dictation; none for a press that stopped it.
    pressed_at: Option<Instant>,
    vocabulary_file: PathBuf,
    /// The vocabulary as read when the current dictation started.
    vocabulary: Vocabulary,
    fillers: Fillers,
    rules: Vec<Rule>,
    /// The rule that governs the current dictation: chosen when it started, none for the
    /// defaults.
    rule: Option<Rule>,
    /// The last finals, as shown; in memory only, for `last`.
    last: VecDeque<String>,
    /// How many of them the current dictation added.
    shown: usize,
    /// All the audio of the current dictation, once the microphone has closed: the engine may
    /// still die of a full GPU while it finishes. In memory only, dropped when it ends.
    heard: Option<Vec<i16>>,
    /// The engine that decodes the dictation again, once its audio is all heard: the GPU had no
    /// memory left for the first one.
    cpu: Option<Arc<dyn Engine>>,
    ledger: Option<Box<dyn Ledger>>,
    /// What is counted of the current dictation, to be kept when it ends.
    tally: Option<Tally>,
    /// When the current dictation was told to stop: the paste is timed from there.
    stopped_at: Option<Instant>,
    media: Option<Pauser>,
}

/// What is kept of a dictation: counts, never its text.
struct Tally {
    app: String,
    words: u32,
    /// The samples the pump has read, which it keeps adding to until the microphone closes.
    samples: Arc<AtomicU64>,
    cpu_fallback: bool,
}

impl Core {
    fn handle(&mut self, message: Message) {
        match message {
            Message::Command(command, reply) => {
                let _ = reply.send(self.command(command));
            }
            Message::Last(reply) => {
                let _ = reply.send(self.last.iter().cloned().collect());
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
            Message::Heard(dictation, audio) if dictation == self.dictation => {
                if self.state != State::Idle {
                    self.heard = Some(audio);
                    self.decode_on_cpu();
                }
            }
            Message::Engine(..) | Message::CaptureEnded(..) | Message::Heard(..) => {}
        }
    }

    fn command(&mut self, command: Command) -> Reply {
        match (&command, self.state) {
            (Command::Status, state) => Ok(state),
            (Command::Press | Command::PressRule(_), State::Idle) => {
                let reply = self.start(true, command.rule());
                if reply.is_ok() {
                    self.pressed_at = Some((self.clock)());
                }
                reply
            }
            (Command::Press | Command::PressRule(_), State::Stopping) => {
                self.pressed_at = None;
                Err("still stopping the last dictation, try again".into())
            }
            (Command::Press | Command::PressRule(_), State::Loading | State::Listening) => {
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
            (Command::Start | Command::StartRule(_) | Command::Toggle, State::Idle) => {
                self.start(true, command.rule())
            }
            (Command::StartNoPaste, State::Idle) => self.start(false, None),
            (
                Command::Start | Command::StartRule(_) | Command::StartNoPaste,
                state @ (State::Loading | State::Listening),
            ) => Ok(state),
            (
                Command::Start | Command::StartRule(_) | Command::StartNoPaste | Command::Toggle,
                State::Stopping,
            ) => Err("still stopping the last dictation, try again".into()),
            (Command::Stop | Command::Toggle, State::Loading | State::Listening) => {
                self.stop.store(true, Ordering::SeqCst);
                self.set(State::Stopping);
                Ok(State::Stopping)
            }
            (Command::Stop, state @ (State::Idle | State::Stopping)) => Ok(state),
        }
    }

    fn start(&mut self, paste: bool, forced: Option<&str>) -> Reply {
        // Before anything is opened: a rule that does not exist is a typo to tell, not a
        // dictation to run with the defaults.
        let forced = forced
            .map(|name| {
                rules::named(&self.rules, name)
                    .cloned()
                    .ok_or_else(|| self.failed(format!("no rule is named {name}")))
            })
            .transpose()?;
        // Before the microphone opens: a model file that is not there stops the engine within
        // milliseconds and leaves nothing to tell why.
        self.engine.check().map_err(|e| self.failed(e))?;
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
        let messages = self.messages.clone();
        thread::spawn(move || feed(input, queue, messages, dictation));
        let samples = Arc::new(AtomicU64::new(0));
        let pump = Pump {
            samples: Arc::clone(&samples),
            capture,
            stop: Arc::clone(&self.stop),
            bus: Arc::clone(&self.bus),
            audio,
            messages: self.messages.clone(),
            dictation,
        };
        thread::spawn(move || pump.run());
        // The window is asked once, where the dictation starts: the rule follows it there, not
        // to where the focus is at the stop. A dictation that pastes nothing has no use for a
        // window rule.
        let paste = if paste { self.paste.as_ref() } else { None };
        let class = paste.and_then(|output| output.begin());
        self.rule = forced.or_else(|| rules::select(&self.rules, class.as_deref()).cloned());
        self.pasting = paste.is_some() && self.rule.as_ref().is_none_or(|rule| rule.paste);
        self.dictated.clear();
        // A dictation whose window was not asked has no application to name.
        self.tally = Some(Tally {
            app: class
                .filter(|class| !class.is_empty())
                .unwrap_or_else(|| stats::UNKNOWN.into()),
            words: 0,
            samples,
            cpu_fallback: false,
        });
        self.shown = 0;
        self.heard = None;
        self.cpu = None;
        self.stopped_at = None;
        self.read_vocabulary();
        // Once nothing can fail any more: a start that is refused pauses nothing.
        if let Some(media) = &self.media {
            media.pause();
        }
        self.set(State::Loading);
        Ok(State::Loading)
    }

    /// A `teach` or an edit by hand is read from the next dictation on, not in the middle of one.
    fn read_vocabulary(&mut self) {
        let (mut vocabulary, mut problems) = Vocabulary::load(&self.vocabulary_file);
        if let Some(rule) = &self.rule {
            let (own, own_problems) = rule.vocabulary();
            vocabulary = vocabulary.with(&own);
            problems.extend(own_problems);
        }
        self.vocabulary = vocabulary;
        if !problems.is_empty() {
            self.bus.publish(&Event::Error {
                message: problems.join("; "),
            });
        }
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
            // Corrected where the engine's text comes in: the partial, the final and the paste
            // all hold the same words. The fillers go after the vocabulary, which may have
            // written a word of its own that the list also names.
            EngineEvent::Partial { utterance, text } => {
                let utterance = self.utterance(utterance);
                let text = self.fillers.drop(&self.vocabulary.correct(&text));
                if let Some(text) = self.partial.read(&text) {
                    self.bus.publish(&Event::Partial { utterance, text });
                }
            }
            EngineEvent::Final { utterance, text } => {
                let utterance = self.utterance(utterance);
                let text = self.fillers.drop(&self.vocabulary.correct(&text));
                self.open_utterance = None;
                // A phrase of only fillers still closes its utterance in the stream, but it is
                // not a phrase to keep or to paste.
                if !text.is_empty() {
                    if self.last.len() == LAST_PHRASES {
                        self.last.pop_front();
                    }
                    self.last.push_back(text.clone());
                    self.shown += 1;
                    if let Some(tally) = &mut self.tally {
                        tally.words += stats::count_words(&text);
                    }
                    if self.pasting {
                        self.dictated.push(text.clone());
                    }
                }
                self.bus.publish(&Event::Final { utterance, text });
            }
            EngineEvent::Silence => self.bus.publish(&Event::Silence),
            // The last final is in; the engine is only unloading its model now, which the
            // paste does not wait for. The state stays `Stopping` until `Ended`. A flush while
            // the dictation goes on is not one: pasting then would cut it in two.
            EngineEvent::Flushed if self.state == State::Stopping => self.paste_dictation(),
            EngineEvent::Flushed => {}
            EngineEvent::Ended(result) => {
                let result = match result {
                    Err(Failure::GpuMemory(line)) => match self.cpu_for_fallback() {
                        Some(cpu) => return self.fall_back(cpu, line),
                        None => Err(format!("the GPU is out of memory: {line}")),
                    },
                    Err(Failure::Other(message)) => Err(message),
                    Ok(()) => Ok(()),
                };
                match result {
                    Err(message) => self.bus.publish(&Event::Error { message }),
                    // No stop was asked, or the state would be `Stopping`: an engine that
                    // leaves by itself must not look like a key that cuts off.
                    Ok(()) if matches!(self.state, State::Loading | State::Listening) => {
                        self.bus.publish(&Event::Error {
                            message: "the engine stopped by itself".into(),
                        })
                    }
                    Ok(()) => {}
                }
                // The engine may have failed while the microphone is still open.
                self.stop.store(true, Ordering::SeqCst);
                self.paste_dictation();
                self.heard = None;
                self.cpu = None;
                self.set(State::Idle);
            }
        }
    }

    /// The engine to decode the dictation again with, if it has not been pasted yet and has not
    /// fallen back already.
    fn cpu_for_fallback(&self) -> Option<Arc<dyn Engine>> {
        self.tally.as_ref().filter(|tally| !tally.cpu_fallback)?;
        self.engine.on_cpu()
    }

    /// The GPU had no memory left for the engine: the whole dictation is decoded again on the
    /// CPU once its audio is all heard, which a dictation still going on waits for. That is a
    /// single decode, through the same paste, and no word spoken after the failure is lost.
    fn fall_back(&mut self, cpu: Arc<dyn Engine>, reason: String) {
        eprintln!(
            "vocaline: the GPU is out of memory, the dictation is decoded on the CPU: {reason}"
        );
        self.bus.publish(&Event::Fallback { reason });
        // The CPU's text holds them: they would be pasted and counted twice.
        self.dictated.clear();
        for _ in 0..self.shown {
            self.last.pop_back();
        }
        self.shown = 0;
        if let Some(tally) = &mut self.tally {
            tally.words = 0;
            tally.cpu_fallback = true;
        }
        self.open_utterance = None;
        self.cpu = Some(cpu);
        if self.state == State::Loading {
            self.set(State::Listening);
        }
        self.decode_on_cpu();
    }

    /// Starts the decode on the CPU once its engine is chosen and the audio all heard, whichever
    /// comes second. Its events are those of the dictation.
    fn decode_on_cpu(&mut self) {
        let (cpu, heard) = match (self.cpu.take(), self.heard.take()) {
            (Some(cpu), Some(heard)) => (cpu, heard),
            (cpu, heard) => {
                (self.cpu, self.heard) = (cpu, heard);
                return;
            }
        };
        let dictation = self.dictation;
        let messages = self.messages.clone();
        match cpu.open(Box::new(move |event| {
            let _ = messages.send(Message::Engine(dictation, event));
        })) {
            Ok(mut input) => {
                thread::spawn(move || {
                    if input.write(&heard).is_ok() {
                        input.finish();
                    }
                });
            }
            Err(e) => self.engine_event(EngineEvent::Ended(Err(Failure::Other(format!(
                "the engine could not start on the CPU: {e}"
            ))))),
        }
    }

    /// Hands the finals of the dictation that ended to the output, as one text. The stop command
    /// is not the moment: the last open utterance is final only once the engine has flushed it
    /// (`Flushed`), or ended without saying so (`Ended`). The finals are taken, so whichever
    /// comes second pastes nothing: the paste happens once.
    fn paste_dictation(&mut self) {
        let finals = std::mem::take(&mut self.dictated);
        // The join comes before the rule: `drop_period` then acts on the end of the whole text.
        // The finals of the stream and of `last` are left as shown.
        let text = phrases::join(&finals, &self.vocabulary);
        let text = match &self.rule {
            Some(rule) => rule.finish(&text),
            None => text,
        };
        let mut pasted = false;
        if !text.is_empty()
            && let Some(output) = &self.paste
        {
            output.phrase(text);
            if self.rule.as_ref().is_some_and(|rule| rule.enter) {
                output.enter();
            }
            pasted = true;
        }
        self.keep(pasted);
    }

    /// Keeps the counts of the dictation that ended, once. A store that cannot be written is
    /// told, and does not fail the dictation. The message holds no text.
    fn keep(&mut self, pasted: bool) {
        let Some(tally) = self.tally.take() else {
            return;
        };
        let Some(ledger) = &self.ledger else {
            return;
        };
        // A dictation that heard nothing is not a dictation.
        if tally.words == 0 {
            return;
        }
        let dictation = Dictation {
            app: tally.app,
            words: tally.words,
            audio_ms: tally.samples.load(Ordering::SeqCst) * 1000 / u64::from(SAMPLE_RATE),
            paste_ms: self
                .stopped_at
                .filter(|_| pasted)
                .map(|at| (self.clock)().saturating_duration_since(at).as_millis() as u64),
            cpu_fallback: tally.cpu_fallback,
        };
        if let Err(e) = ledger.record(&dictation) {
            self.bus.publish(&Event::Error {
                message: format!("the statistics could not be written: {e}"),
            });
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
            if state == State::Stopping {
                self.stopped_at = Some((self.clock)());
            }
            // However the dictation ended; before the stream tells it, so a client that sees
            // `Idle` knows the players have been asked.
            if let (State::Idle, Some(media)) = (state, &self.media) {
                media.resume();
            }
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
    samples: Arc<AtomicU64>,
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
                    self.samples.fetch_add(n as u64, Ordering::SeqCst);
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
/// The audio is kept as well, and handed to the daemon at the end: an engine that died of a full
/// GPU has it all decoded again on the CPU.
fn feed(
    input: Box<dyn EngineInput>,
    queue: mpsc::Receiver<Vec<i16>>,
    messages: mpsc::Sender<Message>,
    dictation: u64,
) {
    let mut input = Some(input);
    let mut heard = Vec::new();
    for chunk in queue {
        heard.extend_from_slice(&chunk);
        // The engine has gone: it reports why through its own events.
        if input
            .as_mut()
            .is_some_and(|input| input.write(&chunk).is_err())
        {
            input = None;
        }
    }
    if let Some(input) = input {
        input.finish();
    }
    let _ = messages.send(Message::Heard(dictation, heard));
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
    if word == "last" {
        let answer = Answer {
            state: None,
            error: None,
            phrases: Some(daemon.last()),
        };
        let answer = serde_json::to_string(&answer).expect("an answer serialises");
        let _ = (&stream).write_all(format!("{answer}\n").as_bytes());
        return;
    }
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
