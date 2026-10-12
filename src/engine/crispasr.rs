//! CrispASR driven as a process: `crispasr --stream` reads raw PCM on its standard input and
//! writes JSON lines on its standard output. The daemon keeps the microphone and feeds it.
//!
//! One process per dictation: the model is loaded when listening starts and unloaded when the
//! process ends, after the end of the audio has finalised the open phrase.
//!
//! Parakeet ignores `-l` and drifts into English on short phrases (`allô` became `Hello?`);
//! Canary obeys it but, streaming, costs ten times the CPU (#17). So a second process holds
//! the short-phrase model while listening, and each phrase shorter than 2 s is
//! decoded again by it, with a second of audio around it: 1 of 144 short French clips came
//! out in another language, against 17 for Parakeet alone, for about 35 ms each.

use super::{Engine, EngineEvent, EngineInput, Sink};
use crate::config::{Backend, EngineConfig};
use serde::Deserialize;
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

mod server;

/// Lines of the engine's diagnostics kept to explain a failure; the rest is dropped.
const KEPT_DIAGNOSTICS: usize = 12;

const RATE: f64 = 16000.0;
/// Phrases shorter than this are decoded again: they are the ones that drift, and the stream
/// does not decode them again for their final. A longer phrase gets that final decoding on all
/// of it; given to Canary too, phrases of 3.5 s came out no more French, and changed
/// (`compte-rendu`, `Pensez`), 60 ms later.
const SHORT_PHRASE_S: f64 = 2.0;
/// The audio around a short phrase: on the phrase alone, Canary still wrote 5 of 144 clips in
/// another language; with a second on each side, 1.
const AROUND_S: f64 = 1.0;
/// The audio kept for that, from the end: a short phrase is finalised within a second or two.
const KEPT_AUDIO: usize = 20 * 16000;

/// Decodes one phrase again; `None` keeps the stream's text.
trait Redecode: Send {
    fn transcribe(&mut self, pcm: &[i16]) -> Option<String>;
}

pub struct CrispAsr {
    config: EngineConfig,
}

impl CrispAsr {
    pub fn new(config: EngineConfig) -> CrispAsr {
        CrispAsr { config }
    }

    fn command(&self) -> Command {
        let c = &self.config;
        let mut command = Command::new(&c.command);
        // No `--backend`: crispasr reads the engine family from the model file, so Parakeet
        // (light, but it ignores `-l` and drifts into English on short phrases) and Canary
        // (which obeys `-l`, for several times the CPU) are one line of configuration apart.
        command
            .args(["--stream", "--stream-json", "-m"])
            .arg(&c.model)
            .args(["-l", &c.language, "--gpu-backend", c.backend.name()])
            .args(["--vad", "--vad-model", &c.vad_model])
            .args(["--stream-step", &c.partial_ms.to_string()])
            // The partial is decoded from this rolling window: shorter than an utterance, it
            // drops the utterance's start. 60 s is where crispasr cuts an utterance anyway.
            .args(["--stream-length", "60000"])
            // Each step decodes only the last 6 to 9 s and keeps the text before them, so a
            // step costs the same at minute one as at second five. 4 s misread words, 8 s
            // cost a third more CPU for no better text.
            .args(["--stream-partial-tail-sec", "6"])
            // A breath shorter than the pause that ends a phrase no longer splits it into
            // slices: crispasr caches at most 8 of them, then decodes them all at every step.
            // crispasr clamps the gap just below the final silence.
            .args(["--stream-vad-merge-gap-ms", &c.final_silence_ms.to_string()])
            .args([
                "--stream-final-on-silence-ms",
                &c.final_silence_ms.to_string(),
            ]);
        one_thread_on_a_gpu(&mut command, c.backend);
        command
    }

    fn server_command(&self, model: &Path, port: u16) -> Command {
        let c = &self.config;
        let mut command = Command::new(&c.command);
        command
            .args([
                "--server",
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .arg("-m")
            .arg(model)
            .args(["-l", &c.language, "--gpu-backend", c.backend.name(), "-np"]);
        one_thread_on_a_gpu(&mut command, c.backend);
        command
    }

    fn open_with(
        &self,
        sink: Sink,
        mut short: Option<Box<dyn Redecode>>,
    ) -> io::Result<Box<dyn EngineInput>> {
        let mut child = self
            .command()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                io::Error::new(e.kind(), format!("{}: {e}", self.config.command.display()))
            })?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        let diagnostics = Arc::new(Mutex::new(VecDeque::new()));
        let kept = Arc::clone(&diagnostics);
        let diagnostics_thread = thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut kept = kept.lock().unwrap();
                kept.push_back(line);
                if kept.len() > KEPT_DIAGNOSTICS {
                    kept.pop_front();
                }
            }
        });

        let heard = Arc::new(Mutex::new(Heard::default()));
        let audio = Arc::clone(&heard);
        let final_silence_s = f64::from(self.config.final_silence_ms) / 1000.0;
        thread::spawn(move || {
            let mut ready = false;
            let mut previous_end = 0.0;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Some(mut event) = parse_line(&line) else {
                    continue;
                };
                // The first event comes after the first step: the model is loaded by then.
                if !ready {
                    ready = true;
                    sink(EngineEvent::Ready);
                }
                if let (EngineEvent::Final { text, .. }, Some((t0, t1))) =
                    (&mut event, times(&line))
                {
                    if let Some(short) = short.as_mut()
                        && t1 - t0 < SHORT_PHRASE_S
                    {
                        // Before: never the previous phrase. After: the silence that ended it.
                        let from = (t0 - AROUND_S).max(previous_end);
                        let to = t1 + AROUND_S.min(final_silence_s);
                        let pcm = audio.lock().unwrap().between(from, to);
                        if let Some(again) = short.transcribe(&pcm) {
                            *text = again;
                        }
                    }
                    previous_end = t1;
                }
                sink(event);
            }
            // Unloaded with the stream's model, before the stream says it ended.
            drop(short);
            let _ = diagnostics_thread.join();
            let result = match child.wait() {
                Ok(status) if status.success() => Ok(()),
                Ok(status) => {
                    let kept = diagnostics.lock().unwrap();
                    let last: Vec<&str> = kept.iter().map(String::as_str).collect();
                    Err(format!(
                        "the engine stopped ({status}): {}",
                        last.join(" | ")
                    ))
                }
                Err(e) => Err(format!("the engine could not be awaited: {e}")),
            };
            sink(EngineEvent::Ended(result));
        });
        Ok(Box::new(Input { stdin, heard }))
    }
}

// The bundled OpenMP pool busy-waits on every core while the GPU decodes: 38 s of CPU for 17 s
// of audio, against about 1 s with a single thread. On the CPU backend those threads do the
// work, so they are left alone.
fn one_thread_on_a_gpu(command: &mut Command, backend: Backend) {
    if backend != Backend::Cpu {
        command.env("OMP_NUM_THREADS", "1");
    }
}

/// The last seconds of audio given to the stream, and where they start in it.
#[derive(Default)]
struct Heard {
    start: usize,
    samples: VecDeque<i16>,
}

impl Heard {
    fn push(&mut self, pcm: &[i16]) {
        self.samples.extend(pcm);
        let excess = self.samples.len().saturating_sub(KEPT_AUDIO);
        self.samples.drain(..excess);
        self.start += excess;
    }

    /// The audio from `from` to `to`, in seconds since the stream started, as far as it is kept.
    fn between(&self, from: f64, to: f64) -> Vec<i16> {
        let index = |t: f64| {
            ((t.max(0.0) * RATE) as usize)
                .saturating_sub(self.start)
                .min(self.samples.len())
        };
        self.samples
            .range(index(from)..index(to))
            .copied()
            .collect()
    }
}

impl Engine for CrispAsr {
    fn open(&self, sink: Sink) -> io::Result<Box<dyn EngineInput>> {
        let short = match &self.config.short_model {
            Some(model) => Some(Box::new(server::Server::spawn(
                |port| self.server_command(model, port),
                &self.config.language,
            )?) as Box<dyn Redecode>),
            None => None,
        };
        self.open_with(sink, short)
    }
}

struct Input {
    stdin: ChildStdin,
    heard: Arc<Mutex<Heard>>,
}

impl EngineInput for Input {
    fn write(&mut self, pcm: &[i16]) -> io::Result<()> {
        self.heard.lock().unwrap().push(pcm);
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        self.stdin.write_all(&bytes)
    }

    /// Closing the standard input is how crispasr learns the audio ended.
    fn finish(self: Box<Self>) {}
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    utterance_id: u64,
    #[serde(default)]
    text: String,
}

/// Where a final phrase starts and ends, in seconds since the stream started.
fn times(line: &str) -> Option<(f64, f64)> {
    #[derive(Deserialize)]
    struct Times {
        t0: f64,
        t1: f64,
    }
    let times: Times = serde_json::from_str(line).ok()?;
    Some((times.t0, times.t1))
}

/// One line of `--stream-json`; anything else the engine prints is not an event.
fn parse_line(line: &str) -> Option<EngineEvent> {
    let line: Line = serde_json::from_str(line).ok()?;
    match line.kind.as_str() {
        "partial" => Some(EngineEvent::Partial {
            utterance: line.utterance_id,
            text: line.text,
        }),
        "final" => Some(EngineEvent::Final {
            utterance: line.utterance_id,
            text: line.text,
        }),
        "silence" => Some(EngineEvent::Silence),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn the_stream_json_lines_become_events() {
        assert_eq!(
            parse_line(
                r#"{"type":"partial","utterance_id":1,"text":"Bonjour.","t0":1.03,"t1":1.6}"#
            ),
            Some(EngineEvent::Partial {
                utterance: 1,
                text: "Bonjour.".into()
            })
        );
        assert_eq!(
            parse_line(
                r#"{"type":"final","utterance_id":2,"text":"Le texte.","t0":6.5,"t1":8.99}"#
            ),
            Some(EngineEvent::Final {
                utterance: 2,
                text: "Le texte.".into()
            })
        );
        assert_eq!(
            parse_line(r#"{"type":"silence","t":0.400}"#),
            Some(EngineEvent::Silence)
        );
    }

    #[test]
    fn what_is_not_an_event_is_ignored() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("crispasr[stream]: reading raw s16le"), None);
        assert_eq!(parse_line(r#"{"type":"something-new"}"#), None);
    }

    #[test]
    fn the_command_carries_the_configuration() {
        let config = EngineConfig {
            command: "/opt/crispasr".into(),
            model: "/models/p.gguf".into(),
            partial_ms: 300,
            final_silence_ms: 900,
            ..EngineConfig::default()
        };
        let command = CrispAsr::new(config).command();
        assert_eq!(command.get_program(), "/opt/crispasr");
        let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        let joined = args.join(" ");
        for expected in [
            "--stream",
            "--stream-json",
            "-m /models/p.gguf",
            "-l fr",
            "--gpu-backend cuda",
            "--vad --vad-model auto",
            "--stream-step 300",
            "--stream-final-on-silence-ms 900",
        ] {
            assert!(
                joined.contains(expected),
                "{expected} missing from {joined}"
            );
        }
        assert!(!args.contains(&"--mic"), "the daemon owns the microphone");
    }

    // A minute-long utterance lost its start from the partial after 30 s, and each step decoded
    // all of it again: 22 s of CPU for 63 s of audio, measured.
    #[test]
    fn a_long_utterance_keeps_its_start_in_the_partial_at_a_bounded_cost() {
        let config = EngineConfig {
            final_silence_ms: 900,
            ..EngineConfig::default()
        };
        let command = CrispAsr::new(config).command();
        let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        let joined = args.join(" ");
        for expected in [
            "--stream-length 60000",
            "--stream-partial-tail-sec 6",
            "--stream-vad-merge-gap-ms 900",
        ] {
            assert!(
                joined.contains(expected),
                "{expected} missing from {joined}"
            );
        }
    }

    // Parakeet ignores `-l fr` and wrote `Hello` for `allô`; Canary obeys it. Choosing the
    // model file must be enough to switch, so the command names no engine family.
    #[test]
    fn short_french_utterances_can_use_a_model_that_obeys_the_language() {
        let config = EngineConfig {
            model: "/models/canary-1b-v2-q8_0.gguf".into(),
            ..EngineConfig::default()
        };
        let command = CrispAsr::new(config).command();
        let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(
            !args.contains(&"--backend"),
            "crispasr reads the engine family from the model file: {args:?}"
        );
        assert!(
            args.join(" ")
                .contains("-m /models/canary-1b-v2-q8_0.gguf -l fr")
        );
    }

    // OpenMP threads spin while the GPU works: 38 s of CPU for 17 s of audio, measured.
    #[test]
    fn on_a_gpu_the_engine_runs_a_single_cpu_thread_pool() {
        let gpu = CrispAsr::new(EngineConfig::default()).command();
        let omp = |c: &Command| {
            c.get_envs()
                .find(|(k, _)| *k == "OMP_NUM_THREADS")
                .and_then(|(_, v)| v.map(|v| v.to_owned()))
        };
        assert_eq!(omp(&gpu), Some("1".into()));
        let cpu = CrispAsr::new(EngineConfig {
            backend: Backend::Cpu,
            ..EngineConfig::default()
        })
        .command();
        assert_eq!(omp(&cpu), None);
    }

    /// A stand-in for `crispasr`: a shell script, so the plumbing is tested without a GPU.
    fn fake_crispasr(name: &str, script: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("vocaline-engine-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn run(script_name: &str, script: &str, audio: &[i16]) -> Vec<EngineEvent> {
        let config = EngineConfig {
            command: fake_crispasr(script_name, script),
            short_model: None,
            ..EngineConfig::default()
        };
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let mut input = CrispAsr::new(config)
            .open(Box::new(move |e| tx.lock().unwrap().send(e).unwrap()))
            .unwrap();
        input.write(audio).unwrap();
        input.finish();
        let mut events = Vec::new();
        loop {
            let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let ended = matches!(event, EngineEvent::Ended(_));
            events.push(event);
            if ended {
                return events;
            }
        }
    }

    #[test]
    fn the_end_of_the_audio_finalises_the_open_phrase() {
        // Like crispasr: a partial while audio comes, the final once the input is closed.
        let script = r#"
            echo 'crispasr[stream]: reading raw s16le 16kHz mono PCM from stdin' >&2
            echo '{"type":"partial","utterance_id":1,"text":"Bonjour"}'
            bytes=$(wc -c)
            echo "{\"type\":\"final\",\"utterance_id\":1,\"text\":\"$bytes bytes\"}"
        "#;
        let events = run("finalises", script, &[1, 2, 3]);
        assert_eq!(
            events,
            [
                EngineEvent::Ready,
                EngineEvent::Partial {
                    utterance: 1,
                    text: "Bonjour".into()
                },
                EngineEvent::Final {
                    utterance: 1,
                    text: "6 bytes".into()
                },
                EngineEvent::Ended(Ok(())),
            ]
        );
    }

    #[test]
    fn an_engine_that_fails_ends_with_its_diagnostics() {
        let script = "cat >/dev/null; echo 'error: model not found' >&2; exit 3";
        let events = run("fails", script, &[0; 16]);
        let [EngineEvent::Ended(Err(message))] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert!(message.contains("model not found"), "{message}");
    }

    #[test]
    fn a_missing_binary_is_an_error_at_open() {
        let config = EngineConfig {
            command: "/nonexistent/crispasr".into(),
            ..EngineConfig::default()
        };
        assert!(CrispAsr::new(config).open(Box::new(|_| {})).is_err());
    }

    /// Stands for the short-phrase model: answers `text` and keeps the audio it was given.
    struct FakeShort {
        text: Option<String>,
        heard: Arc<Mutex<Vec<Vec<i16>>>>,
    }

    impl Redecode for FakeShort {
        fn transcribe(&mut self, pcm: &[i16]) -> Option<String> {
            self.heard.lock().unwrap().push(pcm.to_vec());
            self.text.clone()
        }
    }

    /// Runs `script` as the stream with `short` beside it; the audio counts tenths of a second.
    fn run_with_short(
        name: &str,
        script: &str,
        seconds: usize,
        text: Option<&str>,
    ) -> (Vec<EngineEvent>, Vec<Vec<i16>>) {
        let config = EngineConfig {
            command: fake_crispasr(name, script),
            ..EngineConfig::default()
        };
        let heard = Arc::new(Mutex::new(Vec::new()));
        let short = FakeShort {
            text: text.map(String::from),
            heard: Arc::clone(&heard),
        };
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let mut input = CrispAsr::new(config)
            .open_with(
                Box::new(move |e| tx.lock().unwrap().send(e).unwrap()),
                Some(Box::new(short)),
            )
            .unwrap();
        let audio: Vec<i16> = (0..seconds * 16000).map(|i| (i / 1600) as i16).collect();
        input.write(&audio).unwrap();
        input.finish();
        let mut events = Vec::new();
        loop {
            let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let ended = matches!(event, EngineEvent::Ended(_));
            if !matches!(event, EngineEvent::Ready) {
                events.push(event);
            }
            if ended {
                let heard = heard.lock().unwrap().clone();
                return (events, heard);
            }
        }
    }

    fn final_(utterance: u64, text: &str) -> EngineEvent {
        EngineEvent::Final {
            utterance,
            text: text.into(),
        }
    }

    #[test]
    fn a_short_phrase_is_decoded_again_with_a_second_of_audio_around_it() {
        let script = r#"
            cat >/dev/null
            echo '{"type":"final","utterance_id":1,"text":"Hello?","t0":1.500,"t1":2.200}'
        "#;
        let (events, heard) = run_with_short("short", script, 4, Some("Allô ?"));
        assert_eq!(events, [final_(1, "Allô ?"), EngineEvent::Ended(Ok(()))]);
        // From 0.5 s to 2.2 s plus the final silence (700 ms), shorter than a second.
        let [pcm] = heard.as_slice() else {
            panic!("{heard:?}")
        };
        assert_eq!(pcm.len(), (29 - 5) * 1600);
        assert_eq!((pcm[0], pcm[pcm.len() - 1]), (5, 28));
    }

    #[test]
    fn the_audio_before_a_short_phrase_stops_at_the_end_of_the_previous_one() {
        let script = r#"
            cat >/dev/null
            echo '{"type":"final","utterance_id":1,"text":"Un.","t0":0.200,"t1":0.800}'
            echo '{"type":"final","utterance_id":2,"text":"Deux.","t0":1.200,"t1":1.600}'
        "#;
        let (_, heard) = run_with_short("previous", script, 3, Some("Texte."));
        assert_eq!(heard.len(), 2);
        assert_eq!(heard[1][0], 8, "starts where the previous phrase ended");
    }

    #[test]
    fn a_long_phrase_keeps_the_text_of_the_stream() {
        let script = r#"
            cat >/dev/null
            echo '{"type":"final","utterance_id":1,"text":"Une longue phrase.","t0":0.500,"t1":2.600}'
        "#;
        let (events, heard) = run_with_short("long", script, 4, Some("Autre."));
        assert_eq!(
            events,
            [final_(1, "Une longue phrase."), EngineEvent::Ended(Ok(()))]
        );
        assert!(heard.is_empty());
    }

    #[test]
    fn a_short_model_that_does_not_answer_leaves_the_text_of_the_stream() {
        let script = r#"
            cat >/dev/null
            echo '{"type":"final","utterance_id":1,"text":"Oui.","t0":0.500,"t1":1.000}'
        "#;
        let (events, _) = run_with_short("silent", script, 2, None);
        assert_eq!(events, [final_(1, "Oui."), EngineEvent::Ended(Ok(()))]);
    }

    #[test]
    fn the_short_model_runs_as_a_server_on_the_same_engine_and_language() {
        let config = EngineConfig {
            command: "/opt/crispasr".into(),
            short_model: Some("/models/c.gguf".into()),
            ..EngineConfig::default()
        };
        let command = CrispAsr::new(config).server_command(Path::new("/models/c.gguf"), 4242);
        assert_eq!(command.get_program(), "/opt/crispasr");
        let joined = command
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect::<Vec<_>>()
            .join(" ");
        for expected in [
            "--server --host 127.0.0.1 --port 4242",
            "-m /models/c.gguf",
            "-l fr",
            "--gpu-backend cuda",
        ] {
            assert!(
                joined.contains(expected),
                "{expected} missing from {joined}"
            );
        }
        assert!(
            command
                .get_envs()
                .any(|(k, v)| k == "OMP_NUM_THREADS" && v == Some("1".as_ref()))
        );
    }
}
