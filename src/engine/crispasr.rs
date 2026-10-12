//! CrispASR driven as a process: `crispasr --stream` reads raw PCM on its standard input and
//! writes JSON lines on its standard output. The daemon keeps the microphone and feeds it.
//!
//! One process per dictation: the model is loaded when listening starts and unloaded when the
//! process ends, after the end of the audio has finalised the open phrase.

use super::{Engine, EngineEvent, EngineInput, Sink};
use crate::config::{Backend, EngineConfig};
use serde::Deserialize;
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

/// Lines of the engine's diagnostics kept to explain a failure; the rest is dropped.
const KEPT_DIAGNOSTICS: usize = 12;

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
        // The bundled OpenMP pool busy-waits on every core while the GPU decodes: 38 s of CPU
        // for 17 s of audio, against about 1 s with a single thread. On the CPU backend those
        // threads do the work, so they are left alone.
        if c.backend != Backend::Cpu {
            command.env("OMP_NUM_THREADS", "1");
        }
        command
    }
}

impl Engine for CrispAsr {
    fn open(&self, sink: Sink) -> io::Result<Box<dyn EngineInput>> {
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

        thread::spawn(move || {
            let mut ready = false;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Some(event) = parse_line(&line) else {
                    continue;
                };
                // The first event comes after the first step: the model is loaded by then.
                if !ready {
                    ready = true;
                    sink(EngineEvent::Ready);
                }
                sink(event);
            }
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
        Ok(Box::new(Input { stdin }))
    }
}

struct Input {
    stdin: ChildStdin,
}

impl EngineInput for Input {
    fn write(&mut self, pcm: &[i16]) -> io::Result<()> {
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

/// One line of `--stream-json`; anything else the engine prints is not an event.
fn parse_line(line: &str) -> Option<EngineEvent> {
    // After the audio ended and the open utterance was flushed as a final, crispasr prints one
    // empty line, and only then unloads the model. Nothing else prints an empty line in a
    // stream, and it comes after every final, so it tells that the last one is in. If it were
    // held back until the exit, the paste would just wait for `Ended`, as it did.
    if line.trim().is_empty() {
        return Some(EngineEvent::Flushed);
    }
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
    use std::time::{Duration, Instant};

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
    fn the_empty_line_after_the_last_final_is_the_flush() {
        assert_eq!(parse_line(""), Some(EngineEvent::Flushed));
        assert_eq!(parse_line("  \t"), Some(EngineEvent::Flushed));
    }

    #[test]
    fn what_is_not_an_event_is_ignored() {
        assert_eq!(parse_line("crispasr[stream]: reading raw s16le"), None);
        assert_eq!(parse_line(r#"{"type":"something-new"}"#), None);
    }

    #[test]
    fn the_command_carries_the_configuration() {
        let config = EngineConfig {
            command: "/opt/crispasr".into(),
            model: "/models/p.gguf".into(),
            vad_model: "auto".into(),
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
        run_timed(script_name, script, audio)
            .into_iter()
            .map(|(_, event)| event)
            .collect()
    }

    fn run_timed(script_name: &str, script: &str, audio: &[i16]) -> Vec<(Instant, EngineEvent)> {
        let config = EngineConfig {
            command: fake_crispasr(script_name, script),
            ..EngineConfig::default()
        };
        let (tx, rx) = mpsc::channel();
        // Another test thread may fork while the script is still open for writing: the child
        // holds that descriptor until its own exec, and the kernel refuses to run a file
        // someone has open for writing (ETXTBSY). That ends as soon as the child has exec'd, so
        // the open is tried again; it succeeds the first time on a machine that is not busy.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut input = loop {
            let tx = Mutex::new(tx.clone());
            match CrispAsr::new(config.clone())
                .open(Box::new(move |e| tx.lock().unwrap().send(e).unwrap()))
            {
                Err(e)
                    if e.kind() == io::ErrorKind::ExecutableFileBusy
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                result => break result.unwrap(),
            }
        };
        input.write(audio).unwrap();
        input.finish();
        let mut events = Vec::new();
        loop {
            let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let ended = matches!(event, EngineEvent::Ended(_));
            events.push((Instant::now(), event));
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

    // Like crispasr: the flush of the open utterance, the empty line, then the model is unloaded
    // before the process exits. The flush must reach the daemon without waiting for the exit.
    #[test]
    fn the_flush_is_told_before_the_engine_has_exited() {
        let script = r#"
            cat >/dev/null
            echo '{"type":"final","utterance_id":1,"text":"Bonjour."}'
            echo
            sleep 1
        "#;
        let events = run_timed("flushes", script, &[1, 2, 3]);
        let kinds: Vec<&EngineEvent> = events.iter().map(|(_, e)| e).collect();
        assert_eq!(
            kinds,
            [
                &EngineEvent::Ready,
                &EngineEvent::Final {
                    utterance: 1,
                    text: "Bonjour.".into()
                },
                &EngineEvent::Flushed,
                &EngineEvent::Ended(Ok(())),
            ]
        );
        let wait = events[3].0 - events[2].0;
        assert!(wait >= Duration::from_millis(500), "{wait:?}");
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
}
