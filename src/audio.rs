//! Where the audio comes from: 16 kHz mono signed 16-bit samples, behind a trait so the daemon
//! is tested without a microphone.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const SAMPLE_RATE: u32 = 16_000;

/// Lines of the recorder's diagnostics kept to explain a failure; the rest is dropped.
const KEPT_DIAGNOSTICS: usize = 12;

pub trait Microphone: Send + Sync {
    fn open(&self) -> io::Result<Box<dyn Capture>>;
}

pub trait Capture: Send {
    /// Blocks until `buf` is full; fewer samples only at the end, 0 once the capture ended.
    fn read(&mut self, buf: &mut [i16]) -> io::Result<usize>;
}

/// The default PipeWire source, through `pw-record`: no audio library to link, and the
/// capture ends with the process.
pub struct PwRecord;

impl PwRecord {
    fn command(program: &Path) -> Command {
        let mut command = Command::new(program);
        command.args([
            "--raw",
            "--rate",
            "16000",
            "--channels",
            "1",
            "--format",
            "s16",
            "-",
        ]);
        command
    }
}

impl PwRecord {
    fn spawn(program: &Path) -> io::Result<Box<dyn Capture>> {
        let mut child = PwRecord::command(program)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("pw-record: {e}")))?;
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
        Ok(Box::new(PwCapture {
            child,
            stdout,
            heard: false,
            diagnostics,
            diagnostics_thread: Some(diagnostics_thread),
        }))
    }
}

impl Microphone for PwRecord {
    fn open(&self) -> io::Result<Box<dyn Capture>> {
        PwRecord::spawn(Path::new("pw-record"))
    }
}

struct PwCapture {
    child: Child,
    stdout: ChildStdout,
    heard: bool,
    diagnostics: Arc<Mutex<VecDeque<String>>>,
    diagnostics_thread: Option<JoinHandle<()>>,
}

impl PwCapture {
    /// The recorder closed its output on its own: that is only a clean end if it said so and
    /// had recorded something. A normal stop kills it, and nobody reads after that.
    fn ended(&mut self) -> io::Result<usize> {
        let status = self.child.wait()?;
        if let Some(thread) = self.diagnostics_thread.take() {
            let _ = thread.join();
        }
        if status.success() && self.heard {
            return Ok(0);
        }
        let kept = self.diagnostics.lock().unwrap();
        let last: Vec<&str> = kept.iter().map(String::as_str).collect();
        let why = if status.success() {
            "ended before any audio".to_string()
        } else {
            format!("stopped ({status})")
        };
        Err(io::Error::other(format!(
            "pw-record {why}: {}",
            last.join(" | ")
        )))
    }
}

impl Capture for PwCapture {
    fn read(&mut self, buf: &mut [i16]) -> io::Result<usize> {
        let mut bytes = vec![0u8; buf.len() * 2];
        let mut filled = 0;
        while filled < bytes.len() {
            match self.stdout.read(&mut bytes[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        if filled == 0 {
            return self.ended();
        }
        self.heard = true;
        for (sample, pair) in buf.iter_mut().zip(bytes[..filled].as_chunks::<2>().0) {
            *sample = i16::from_le_bytes(*pair);
        }
        Ok(filled / 2)
    }
}

impl Drop for PwCapture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A recording played as if spoken now, at the pace of real time: to try and measure the
/// daemon without a microphone. Each dictation plays it from the start, then ends.
pub struct WavFile {
    path: PathBuf,
}

impl WavFile {
    pub fn new(path: &Path) -> WavFile {
        WavFile { path: path.into() }
    }
}

impl Microphone for WavFile {
    fn open(&self) -> io::Result<Box<dyn Capture>> {
        let samples = parse_wav(&std::fs::read(&self.path)?).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {e}", self.path.display()),
            )
        })?;
        Ok(Box::new(Playback {
            samples,
            position: 0,
            started: Instant::now(),
        }))
    }
}

struct Playback {
    samples: Vec<i16>,
    position: usize,
    started: Instant,
}

impl Capture for Playback {
    fn read(&mut self, buf: &mut [i16]) -> io::Result<usize> {
        let n = buf.len().min(self.samples.len() - self.position);
        buf[..n].copy_from_slice(&self.samples[self.position..self.position + n]);
        self.position += n;
        // A microphone hands its samples over once they have been spoken.
        let due = Duration::from_secs_f64(self.position as f64 / f64::from(SAMPLE_RATE));
        if let Some(wait) = due.checked_sub(self.started.elapsed()) {
            std::thread::sleep(wait);
        }
        Ok(n)
    }
}

/// The samples of a 16 kHz mono 16-bit PCM WAV file.
fn parse_wav(bytes: &[u8]) -> Result<Vec<i16>, String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let mut format = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let size = u32_at(at + 4) as usize;
        let body = at + 8;
        let end = (body + size).min(bytes.len());
        match &bytes[at..at + 4] {
            b"fmt " if size >= 16 => {
                format = Some((
                    u16_at(body),
                    u16_at(body + 2),
                    u32_at(body + 4),
                    u16_at(body + 14),
                ))
            }
            b"data" => {
                if format != Some((1, 1, SAMPLE_RATE, 16)) {
                    return Err("not 16 kHz mono 16-bit PCM; convert it with \
                         ffmpeg -i <in> -ar 16000 -ac 1 -sample_fmt s16 <out.wav>"
                        .into());
                }
                return Ok(bytes[body..end]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| i16::from_le_bytes(*pair))
                    .collect());
            }
            _ => {}
        }
        // Chunks are padded to an even size.
        at = body + size + size % 2;
    }
    Err("no audio data in the WAV file".into())
}

/// The loudness of `samples`, from 0 (silence) to 1 (full scale).
pub fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    ((sum / samples.len() as f64).sqrt() / 32_768.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, channels: u16, bits: u16, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        out.extend(b"RIFF");
        out.extend((36 + data.len() as u32).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(channels.to_le_bytes());
        out.extend(rate.to_le_bytes());
        out.extend((rate * u32::from(channels) * u32::from(bits) / 8).to_le_bytes());
        out.extend((channels * bits / 8).to_le_bytes());
        out.extend(bits.to_le_bytes());
        // A chunk the reader must skip, as ffmpeg writes one.
        out.extend(b"LIST");
        out.extend(4u32.to_le_bytes());
        out.extend(b"INFO");
        out.extend(b"data");
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        out
    }

    #[test]
    fn a_16_khz_mono_wav_gives_its_samples() {
        assert_eq!(
            parse_wav(&wav(16_000, 1, 16, &[1, -2, 300])).unwrap(),
            [1, -2, 300]
        );
    }

    #[test]
    fn another_wav_format_is_refused_with_the_way_to_convert_it() {
        let error = parse_wav(&wav(48_000, 2, 16, &[0; 4])).unwrap_err();
        assert!(error.contains("ffmpeg"), "{error}");
        assert!(parse_wav(b"not a wav").is_err());
    }

    #[test]
    fn a_recording_plays_at_the_pace_of_real_time_then_ends() {
        let path = std::env::temp_dir().join(format!("vocaline-audio-{}.wav", std::process::id()));
        std::fs::write(&path, wav(16_000, 1, 16, &[7; 3200])).unwrap();
        let started = Instant::now();
        let mut capture = WavFile::new(&path).open().unwrap();
        let mut buf = [0; 1600];
        assert_eq!(capture.read(&mut buf).unwrap(), 1600);
        assert_eq!(capture.read(&mut buf).unwrap(), 1600);
        assert_eq!(capture.read(&mut buf).unwrap(), 0);
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(buf[0], 7);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_level_is_zero_for_silence_and_one_at_full_scale() {
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&[0; 100]), 0.0);
        assert!((rms(&[i16::MAX, i16::MIN]) - 1.0).abs() < 0.001);
        assert!((rms(&[16_384, -16_384]) - 0.5).abs() < 0.001);
    }

    #[test]
    fn pw_record_captures_16_khz_mono_raw_samples_on_its_output() {
        let command = PwRecord::command(Path::new("pw-record"));
        assert_eq!(command.get_program(), "pw-record");
        let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args.join(" "),
            "--raw --rate 16000 --channels 1 --format s16 -"
        );
    }

    /// A stand-in for `pw-record`: a shell script, so the capture is tested without PipeWire.
    fn fake_pw_record(name: &str, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let folder = std::env::temp_dir().join(format!("vocaline-pw-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn a_recorder_that_fails_at_once_ends_the_capture_with_its_diagnostics() {
        let script = "echo 'error: connection refused' >&2; echo 'no PipeWire' >&2; exit 1";
        let mut capture = PwRecord::spawn(&fake_pw_record("fails", script)).unwrap();
        let error = capture.read(&mut [0; 160]).unwrap_err().to_string();
        assert!(error.contains("pw-record"), "{error}");
        assert!(error.contains("connection refused"), "{error}");
        assert!(error.contains("no PipeWire"), "{error}");
    }

    #[test]
    fn a_recorder_that_ends_without_any_audio_is_an_error_even_when_it_exits_well() {
        let mut capture = PwRecord::spawn(&fake_pw_record("silent", "exit 0")).unwrap();
        assert!(capture.read(&mut [0; 160]).is_err());
    }

    #[test]
    fn a_recorder_that_ends_after_its_audio_ends_the_capture_quietly() {
        let script = "head -c 640 /dev/zero";
        let mut capture = PwRecord::spawn(&fake_pw_record("clean", script)).unwrap();
        let mut buf = [1; 320];
        assert_eq!(capture.read(&mut buf).unwrap(), 320);
        assert_eq!(capture.read(&mut buf).unwrap(), 0);
    }
}
