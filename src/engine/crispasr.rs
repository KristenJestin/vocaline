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
        command
            .args(["--stream", "--stream-json", "--backend", "parakeet", "-m"])
            .arg(&c.model)
            .args(["-l", &c.language, "--gpu-backend", c.backend.name()])
            .args(["--vad", "--vad-model", &c.vad_model])
            .args(["--stream-step", &c.partial_ms.to_string()])
            .args(["--stream-length", "30000"])
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
            "--backend parakeet",
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
}
