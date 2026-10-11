//! Where the audio comes from: 16 kHz mono signed 16-bit samples, behind a trait so the daemon
//! is tested without a microphone.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

pub const SAMPLE_RATE: u32 = 16_000;

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
    fn command() -> Command {
        let mut command = Command::new("pw-record");
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

impl Microphone for PwRecord {
    fn open(&self) -> io::Result<Box<dyn Capture>> {
        let mut child = PwRecord::command()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("pw-record: {e}")))?;
        let stdout = child.stdout.take().expect("piped");
        Ok(Box::new(PwCapture { child, stdout }))
    }
}

struct PwCapture {
    child: Child,
    stdout: ChildStdout,
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
        let command = PwRecord::command();
        assert_eq!(command.get_program(), "pw-record");
        let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args.join(" "),
            "--raw --rate 16000 --channels 1 --format s16 -"
        );
    }
}
