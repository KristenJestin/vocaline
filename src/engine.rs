//! The speech engine, behind a trait so the daemon is tested with a fake one.

mod crispasr;

pub use crispasr::CrispAsr;

use std::io;

#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// The model is loaded and the engine is transcribing.
    Ready,
    /// `utterance` is the engine's own number, unique within one stream.
    Partial {
        utterance: u64,
        text: String,
    },
    Final {
        utterance: u64,
        text: String,
    },
    Silence,
    /// The last event of a stream: after `finish`, or because the engine failed.
    Ended(Result<(), String>),
}

pub type Sink = Box<dyn Fn(EngineEvent) + Send + Sync>;

pub trait Engine: Send + Sync {
    /// Loads the model and opens a stream of 16 kHz mono audio; its events go to `sink`.
    fn open(&self, sink: Sink) -> io::Result<Box<dyn EngineInput>>;
}

pub trait EngineInput: Send {
    fn write(&mut self, pcm: &[i16]) -> io::Result<()>;

    /// No more audio: the open utterance is finalised, then `Ended` closes the stream.
    fn finish(self: Box<Self>);
}
