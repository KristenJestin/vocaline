//! The event stream: one JSON object per line. It is a contract read by other programs: any
//! change is described in `docs/events.md` and in the pull request that makes it.

use serde::{Deserialize, Serialize};

/// Raised when a line changes meaning or loses a field; adding a field or an event type does not.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Event {
    /// The first line a watcher receives.
    Hello {
        version: u32,
    },
    State {
        state: State,
    },
    /// The loudness of the last 100 ms of microphone audio, from 0 to 1.
    Level {
        rms: f32,
    },
    /// The phrase being spoken, as heard so far: it keeps changing.
    Partial {
        utterance: u64,
        text: String,
    },
    /// The phrase, frozen at the pause; it closes `utterance`.
    Final {
        utterance: u64,
        text: String,
    },
    /// The engine heard no speech in its last step.
    Silence,
    Error {
        message: String,
    },
    /// The GPU had no memory left for the dictation, which is decoded again on the CPU; its
    /// finals come again from its start.
    Fallback {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Loading,
    Listening,
    Stopping,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Loading => "loading",
            State::Listening => "listening",
            State::Stopping => "stopping",
        }
    }
}

impl Event {
    /// Three decimals are enough to draw a meter and keep the line short.
    pub fn level(rms: f32) -> Event {
        Event::Level {
            rms: (rms.clamp(0.0, 1.0) * 1000.0).round() / 1000.0,
        }
    }

    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("an event always serialises")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exact lines of docs/events.md: a test failing here means the contract changed.
    #[test]
    fn each_event_is_the_line_documented() {
        let cases = [
            (
                Event::Hello { version: 1 },
                r#"{"type":"hello","version":1}"#,
            ),
            (
                Event::State {
                    state: State::Listening,
                },
                r#"{"type":"state","state":"listening"}"#,
            ),
            (Event::level(0.0312), r#"{"type":"level","rms":0.031}"#),
            (
                Event::Partial {
                    utterance: 3,
                    text: "Bonjour, je voudrais".into(),
                },
                r#"{"type":"partial","utterance":3,"text":"Bonjour, je voudrais"}"#,
            ),
            (
                Event::Final {
                    utterance: 3,
                    text: "Bonjour, je voudrais tester.".into(),
                },
                r#"{"type":"final","utterance":3,"text":"Bonjour, je voudrais tester."}"#,
            ),
            (Event::Silence, r#"{"type":"silence"}"#),
            (
                Event::Error {
                    message: "the engine stopped".into(),
                },
                r#"{"type":"error","message":"the engine stopped"}"#,
            ),
            (
                Event::Fallback {
                    reason: "cudaMalloc failed: out of memory".into(),
                },
                r#"{"type":"fallback","reason":"cudaMalloc failed: out of memory"}"#,
            ),
        ];
        for (event, line) in cases {
            assert_eq!(event.to_line(), line);
            assert_eq!(serde_json::from_str::<Event>(line).unwrap(), event);
        }
    }

    #[test]
    fn the_states_are_named_as_in_the_stream() {
        let names = [
            State::Idle,
            State::Loading,
            State::Listening,
            State::Stopping,
        ]
        .map(State::name);
        assert_eq!(names, ["idle", "loading", "listening", "stopping"]);
    }

    #[test]
    fn a_level_stays_between_zero_and_one() {
        assert_eq!(Event::level(1.7), Event::Level { rms: 1.0 });
        assert_eq!(Event::level(-0.1), Event::Level { rms: 0.0 });
    }
}
