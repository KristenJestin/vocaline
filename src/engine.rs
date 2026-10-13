//! The speech engine, behind a trait so the daemon is tested with a fake one.

mod crispasr;

pub use crispasr::CrispAsr;

use std::io;
use std::sync::Arc;

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
    /// Every final of the stream has been sent, whether or not the audio left one open: only the
    /// engine's shutdown (unloading the model) is left. Engines that cannot tell never send it.
    Flushed,
    /// The last event of a stream: after `finish`, or because the engine failed.
    Ended(Result<(), Failure>),
}

/// Why an engine ended before it was asked to.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// The GPU had no memory left for the engine; the line that said so. The same audio can
    /// still be decoded on the CPU.
    GpuMemory(String),
    Other(String),
}

impl From<&str> for Failure {
    fn from(message: &str) -> Failure {
        Failure::Other(message.into())
    }
}

pub type Sink = Box<dyn Fn(EngineEvent) + Send + Sync>;

pub trait Engine: Send + Sync {
    /// Whether what the engine needs is there, asked before a dictation starts; the error
    /// says what is missing and how to get it. An engine with nothing to check says nothing.
    fn check(&self) -> Result<(), String> {
        Ok(())
    }

    /// Loads the model and opens a stream of 16 kHz mono audio; its events go to `sink`.
    fn open(&self, sink: Sink) -> io::Result<Box<dyn EngineInput>>;

    /// The same engine on the CPU, for a stream the GPU had no memory for. None for an engine
    /// that is on the CPU already.
    fn on_cpu(&self) -> Option<Arc<dyn Engine>> {
        None
    }
}

pub trait EngineInput: Send {
    fn write(&mut self, pcm: &[i16]) -> io::Result<()>;

    /// No more audio: the open utterance is finalised, then `Flushed` if the engine says so,
    /// then `Ended` closes the stream.
    fn finish(self: Box<Self>);
}
