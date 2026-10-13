//! CrispASR driven as a process: `crispasr --stream` reads raw PCM on its standard input and
//! writes JSON lines on its standard output. The daemon keeps the microphone and feeds it.
//!
//! One process per dictation: the model is loaded when listening starts and unloaded when the
//! process ends, after the end of the audio has finalised the open phrase.
//!
//! A dictation the GPU had no memory for is decoded again on the CPU as one recording: a WAV on
//! the standard input of `crispasr -f -`, its text on the standard output.

use super::{Engine, EngineEvent, EngineInput, Failure, Sink};
use crate::audio::SAMPLE_RATE;
use crate::config::{Backend, EngineConfig};
use serde::Deserialize;
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// Lines of the engine's diagnostics kept to explain a failure; the rest is dropped.
const KEPT_DIAGNOSTICS: usize = 5;

/// Bytes of them that go into the event: it reaches every watcher, and a log.
const KEPT_BYTES: usize = 400;

pub struct CrispAsr {
    config: EngineConfig,
    /// The audio is decoded as one recording once it has all come, not as a stream.
    whole: bool,
}

impl CrispAsr {
    pub fn new(config: EngineConfig) -> CrispAsr {
        CrispAsr {
            config,
            whole: false,
        }
    }

    /// The same model on the CPU, for a recording the GPU had no memory for. Streamed, its
    /// partials are decoded again at every step: 17 s of dictation took 14 s after the stop and
    /// 61 s of CPU, against 1.5 s and 8 s as one recording, measured.
    fn cpu_twin(&self) -> CrispAsr {
        CrispAsr {
            config: EngineConfig {
                backend: Backend::Cpu,
                ..self.config.clone()
            },
            whole: true,
        }
    }

    fn command(&self) -> Command {
        let c = &self.config;
        let mut command = Command::new(&c.command);
        if self.whole {
            command
                .arg("-m")
                .arg(&c.model)
                .args(["-l", &c.language, "--gpu-backend", c.backend.name()])
                .args(["--vad", "--vad-model", &c.vad_model])
                .args(["-f", "-", "-np"])
                // The GPU had no memory left: not even a CUDA context is asked of it.
                .env("CUDA_VISIBLE_DEVICES", "");
            return command;
        }
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
    /// The files the configuration names, unless the VAD model is left to crispasr. Read from
    /// the disk at each call: `vocaline setup` may have fetched one since the daemon started.
    fn check(&self) -> Result<(), String> {
        let c = &self.config;
        let vad_model = Path::new(&c.vad_model);
        let vad_model = (c.vad_model != "auto").then_some(vad_model);
        for (what, file) in [("model", Some(c.model.as_path())), ("VAD model", vad_model)] {
            let Some(file) = file else { continue };
            match std::fs::metadata(file) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(format!(
                        "the {what} file {} is missing: `vocaline setup` fetches it",
                        file.display()
                    ));
                }
                Err(e) => return Err(format!("the {what} file {}: {e}", file.display())),
            }
        }
        Ok(())
    }

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
        let finished = Arc::new(AtomicBool::new(false));
        let ended_by_us = Arc::clone(&finished);
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        let diagnostics = Arc::new(Mutex::new(VecDeque::new()));
        let kept = Arc::clone(&diagnostics);
        // Seen as it comes: the backtrace of the abort that follows pushes it out of the tail.
        let gpu_full = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&gpu_full);
        let on_gpu = self.config.backend != Backend::Cpu;
        let diagnostics_thread = thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if on_gpu && line.contains("out of memory") {
                    seen.lock().unwrap().get_or_insert_with(|| tail(&line));
                }
                let mut kept = kept.lock().unwrap();
                kept.push_back(line);
                if kept.len() > KEPT_DIAGNOSTICS {
                    kept.pop_front();
                }
            }
        });

        let whole = self.whole;
        thread::spawn(move || {
            let mut ready = false;
            let lines = BufReader::new(stdout).lines().map_while(Result::ok);
            if whole {
                // One line of text for the whole recording; more than one is joined.
                let text = lines.collect::<Vec<_>>().join(" ");
                let text = text.trim();
                if !text.is_empty() {
                    sink(EngineEvent::Final {
                        utterance: 1,
                        text: text.into(),
                    });
                }
            } else {
                for line in lines {
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
            }
            let _ = diagnostics_thread.join();
            let gpu_full = gpu_full.lock().unwrap().take();
            let result = match (child.wait(), gpu_full) {
                // A success while the audio was still coming is no end: the engine gave up.
                (Ok(status), _) if status.success() && ended_by_us.load(Ordering::SeqCst) => Ok(()),
                (Ok(status), Some(line)) if !status.success() => Err(Failure::GpuMemory(line)),
                (Ok(status), _) if status.success() => Err(Failure::Other(format!(
                    "the engine stopped by itself ({status}): {}",
                    tail(&diagnostics_text(&diagnostics))
                ))),
                (Ok(status), _) => Err(Failure::Other(format!(
                    "the engine stopped ({status}): {}",
                    tail(&diagnostics_text(&diagnostics))
                ))),
                (Err(e), _) => Err(Failure::Other(format!(
                    "the engine could not be awaited: {e}"
                ))),
            };
            sink(EngineEvent::Ended(result));
        });
        Ok(Box::new(Input {
            stdin,
            finished,
            recording: self.whole.then(Vec::new),
        }))
    }

    fn on_cpu(&self) -> Option<Arc<dyn Engine>> {
        (self.config.backend != Backend::Cpu).then(|| Arc::new(self.cpu_twin()) as Arc<dyn Engine>)
    }
}

/// The end of `text`, at most `KEPT_BYTES` long, cut on a character: where a process says why
/// it stopped.
fn tail(text: &str) -> String {
    let Some(mut from) = text.len().checked_sub(KEPT_BYTES) else {
        return text.into();
    };
    while !text.is_char_boundary(from) {
        from += 1;
    }
    format!("…{}", &text[from..])
}

/// The kept lines of the engine's diagnostics, on one line.
fn diagnostics_text(kept: &Mutex<VecDeque<String>>) -> String {
    let kept = kept.lock().unwrap();
    kept.iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" | ")
}

struct Input {
    stdin: ChildStdin,
    /// Set once the audio has ended, so the exit that follows is the one that was asked for.
    finished: Arc<AtomicBool>,
    /// The audio of a recording decoded as one, in memory until it has all come: a WAV
    /// header holds its length.
    recording: Option<Vec<i16>>,
}

impl EngineInput for Input {
    fn write(&mut self, pcm: &[i16]) -> io::Result<()> {
        if let Some(recording) = &mut self.recording {
            recording.extend_from_slice(pcm);
            return Ok(());
        }
        self.stdin.write_all(&le_bytes(pcm))
    }

    /// Closing the standard input is how crispasr learns the audio ended.
    fn finish(mut self: Box<Self>) {
        self.finished.store(true, Ordering::SeqCst);
        if let Some(recording) = self.recording.take() {
            let mut wav = wav_header(recording.len());
            wav.extend(le_bytes(&recording));
            // An engine that has gone reports why through its own events.
            let _ = self.stdin.write_all(&wav);
        }
    }
}

fn le_bytes(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// The header of a 16 kHz mono 16-bit WAV of `samples` samples.
fn wav_header(samples: usize) -> Vec<u8> {
    let data = (samples * 2) as u32;
    let mut header = Vec::with_capacity(44);
    header.extend(b"RIFF");
    header.extend((36 + data).to_le_bytes());
    header.extend(b"WAVEfmt ");
    header.extend(16u32.to_le_bytes());
    header.extend(1u16.to_le_bytes()); // PCM
    header.extend(1u16.to_le_bytes()); // mono
    header.extend(SAMPLE_RATE.to_le_bytes());
    header.extend((SAMPLE_RATE * 2).to_le_bytes());
    header.extend(2u16.to_le_bytes());
    header.extend(16u16.to_le_bytes());
    header.extend(b"data");
    header.extend(data.to_le_bytes());
    header
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

    fn open_fake(
        script_name: &str,
        script: &str,
    ) -> (Box<dyn EngineInput>, mpsc::Receiver<EngineEvent>) {
        open_fake_with(script_name, script, CrispAsr::new)
    }

    fn open_fake_with(
        script_name: &str,
        script: &str,
        engine: impl Fn(EngineConfig) -> CrispAsr,
    ) -> (Box<dyn EngineInput>, mpsc::Receiver<EngineEvent>) {
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
        let input = loop {
            let tx = Mutex::new(tx.clone());
            match engine(config.clone())
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
        (input, rx)
    }

    fn run_timed(script_name: &str, script: &str, audio: &[i16]) -> Vec<(Instant, EngineEvent)> {
        let (mut input, rx) = open_fake(script_name, script);
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
        let [EngineEvent::Ended(Err(Failure::Other(message)))] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert!(message.contains("model not found"), "{message}");
    }

    // The tail goes into an event every watcher reads: a few lines, a few hundred bytes, the
    // end of them since that is where a process says why it stopped.
    #[test]
    fn the_diagnostics_of_a_failing_engine_are_a_short_tail() {
        let script = r#"
            cat >/dev/null
            for i in $(seq 1 40); do echo "line $i: $(printf 'x%.0s' $(seq 1 100))" >&2; done
            echo 'error: the end, é' >&2
            exit 3
        "#;
        let events = run("long-tail", script, &[0; 16]);
        let [EngineEvent::Ended(Err(Failure::Other(message)))] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert!(message.ends_with("error: the end, é"), "{message}");
        assert!(!message.contains("line 1:"), "{message}");
        assert!(message.len() <= 500, "{} bytes: {message}", message.len());
    }

    // The real case of #33 was an engine that left with status 0 before the audio ended.
    #[test]
    fn an_engine_that_leaves_by_itself_with_success_is_an_error_with_its_diagnostics() {
        let script = "echo 'warning: no VAD model, giving up' >&2";
        let (input, rx) = open_fake("leaves", script);
        let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let EngineEvent::Ended(Err(Failure::Other(message))) = event else {
            panic!("{event:?}");
        };
        assert!(message.contains("by itself"), "{message}");
        assert!(message.contains("no VAD model"), "{message}");
        drop(input);
    }

    // What crispasr printed on the maintainer's machine, three times in twenty minutes, before
    // its abort; ggml then prints a backtrace, which pushes the memory line out of the tail.
    #[test]
    fn a_gpu_out_of_memory_is_told_apart_from_other_failures() {
        let script = r##"
            cat >/dev/null
            echo 'ggml_backend_cuda_buffer_type_alloc_buffer: allocating 232.73 MiB on device 0: cudaMalloc failed: out of memory' >&2
            echo 'parakeet: failed to alloc encoder graph' >&2
            echo 'crispasr[parakeet]: single-pass encode failed (likely VRAM OOM at 17s); falling back to streamed encoding — pass --chunk-seconds N for segmented output or --att-context L,R for bounded-memory single-pass' >&2
            echo 'ggml/src/ggml-backend.cpp:2433: GGML_ASSERT(...) failed' >&2
            for i in $(seq 1 12); do echo "#$i 0x0000 in ggml_abort ()" >&2; done
            kill -ABRT $$
        "##;
        let events = run("gpu-full", script, &[0; 16]);
        let [EngineEvent::Ended(Err(Failure::GpuMemory(reason)))] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert!(
            reason.ends_with("allocating 232.73 MiB on device 0: cudaMalloc failed: out of memory"),
            "{reason}"
        );
    }

    // Streamed on the CPU, 17 s of dictation took 14 s to decode after the stop and 61 s of CPU,
    // its partials decoded again at every step; decoded as one recording, 1.5 s and 8 s.
    #[test]
    fn the_engine_on_the_cpu_decodes_the_whole_recording_at_once() {
        let config = EngineConfig {
            model: "/models/p.gguf".into(),
            vad_model: "auto".into(),
            ..EngineConfig::default()
        };
        let gpu = CrispAsr::new(config);
        assert!(gpu.on_cpu().is_some());
        let cpu = gpu.cpu_twin();
        assert!(cpu.on_cpu().is_none(), "the CPU is the last resort");
        let command = cpu.command();
        let args: Vec<&str> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        let joined = args.join(" ");
        for expected in [
            "-m /models/p.gguf -l fr --gpu-backend cpu",
            "--vad --vad-model auto",
            "-f - -np",
        ] {
            assert!(
                joined.contains(expected),
                "{expected} missing from {joined}"
            );
        }
        assert!(!args.contains(&"--stream"), "{joined}");
        // A GPU with no memory left is not even asked for a CUDA context.
        let hidden = command
            .get_envs()
            .find(|(k, _)| *k == "CUDA_VISIBLE_DEVICES")
            .and_then(|(_, v)| v.map(|v| v.to_owned()));
        assert_eq!(hidden, Some("".into()));
    }

    #[test]
    fn on_the_cpu_the_audio_goes_as_one_wav_and_its_text_is_one_final() {
        let script = r#"
            header=$(head -c 4)
            rest=$(wc -c)
            echo "Bonjour, $header and $rest bytes."
            echo 'Ça va ?'
        "#;
        let (mut input, rx) =
            open_fake_with("whole", script, |config| CrispAsr::new(config).cpu_twin());
        input.write(&[1, 2, 3]).unwrap();
        input.write(&[4]).unwrap();
        input.finish();
        let events: Vec<EngineEvent> = (0..2)
            .map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect();
        // 40 bytes of header after `RIFF`, then 4 samples of 2 bytes.
        assert_eq!(
            events,
            [
                EngineEvent::Final {
                    utterance: 1,
                    text: "Bonjour, RIFF and 48 bytes. Ça va ?".into()
                },
                EngineEvent::Ended(Ok(())),
            ]
        );
    }

    #[test]
    fn a_successful_exit_after_the_end_of_the_audio_stays_silent() {
        let script = "cat >/dev/null; echo 'info: model unloaded' >&2";
        let events = run("quiet-exit", script, &[0; 16]);
        assert_eq!(events, [EngineEvent::Ended(Ok(()))]);
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
