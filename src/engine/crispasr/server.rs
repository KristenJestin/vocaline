//! The short-phrase model: a second crispasr, run as an HTTP server so its model stays loaded
//! for the whole dictation, asked to decode one short phrase at a time.

use super::Redecode;
use serde::Deserialize;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// The first phrase can come before the model is loaded (about half a second, measured): it
/// waits for it this long at most, then keeps the stream's text.
const PATIENCE: Duration = Duration::from_secs(10);

pub struct Server {
    child: Child,
    port: u16,
    language: String,
}

impl Server {
    /// `command` comes without its port: the server is given a free one.
    pub fn spawn(command: impl FnOnce(u16) -> Command, language: &str) -> io::Result<Server> {
        let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let child = command(port)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Server {
            child,
            port,
            language: language.into(),
        })
    }
}

impl Redecode for Server {
    fn transcribe(&mut self, pcm: &[i16]) -> Option<String> {
        let child = &mut self.child;
        let text = ask(self.port, &self.language, pcm, PATIENCE, || {
            matches!(child.try_wait(), Ok(None))
        });
        if text.is_none() {
            eprintln!("vocaline: the short-phrase model did not answer; the stream's text is kept");
        }
        text
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Posts `pcm` to the server on `port`, waiting while it loads and while `alive` holds.
fn ask(
    port: u16,
    language: &str,
    pcm: &[i16],
    patience: Duration,
    mut alive: impl FnMut() -> bool,
) -> Option<String> {
    let body = form(language, &wav(pcm));
    let deadline = Instant::now() + patience;
    loop {
        match post(port, &body) {
            Ok((200, json)) => {
                #[derive(Deserialize)]
                struct Answer {
                    text: String,
                }
                let text = serde_json::from_str::<Answer>(&json).ok()?.text;
                let text = text.trim();
                return (!text.is_empty()).then(|| text.to_string());
            }
            // 503 while the model loads; refused before the server listens.
            Ok((503, _)) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {}
            _ => return None,
        }
        if Instant::now() >= deadline || !alive() {
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

const BOUNDARY: &str = "vocaline-phrase";

fn form(language: &str, wav: &[u8]) -> Vec<u8> {
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"language\"\r\n\r\n{language}\r\n\
         --{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"phrase.wav\"\r\n\
         Content-Type: audio/wav\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn post(port: u16, body: &[u8]) -> io::Result<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(PATIENCE))?;
    write!(
        stream,
        "POST /inference HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Content-Type: multipart/form-data; boundary={BOUNDARY}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "not an HTTP response");
    let (head, json) = response.split_once("\r\n\r\n").ok_or_else(invalid)?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(invalid)?;
    Ok((status, json.to_string()))
}

/// 16 kHz mono 16-bit PCM, as a WAV file.
fn wav(pcm: &[i16]) -> Vec<u8> {
    let data = (pcm.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&16000u32.to_le_bytes());
    wav.extend_from_slice(&32000u32.to_le_bytes()); // bytes per second
    wav.extend_from_slice(&2u16.to_le_bytes()); // bytes per frame
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data.to_le_bytes());
    for sample in pcm {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::io::BufReader;

    /// A stand-in for `crispasr --server`: answers each request with the next of `answers`,
    /// and gives back the requests it read.
    fn fake_server(answers: Vec<(u16, &'static str)>) -> (u16, thread::JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, json) in answers {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut length = 0;
                let mut request = Vec::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    request.extend_from_slice(line.as_bytes());
                    if let Some(n) = line.strip_prefix("Content-Length: ") {
                        length = n.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                request.extend_from_slice(&body);
                requests.push(request);
                let mut stream = reader.into_inner();
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{json}",
                    json.len()
                )
                .unwrap();
            }
            requests
        });
        (port, handle)
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn the_phrase_is_posted_as_a_wav_file_with_the_language() {
        let (port, server) = fake_server(vec![(200, r#"{"text":" Allô ? ","segments":[]}"#)]);
        let text = ask(port, "fr", &[1, -1], PATIENCE, || true);
        assert_eq!(text.as_deref(), Some("Allô ?"));
        let requests = server.join().unwrap();
        let request = &requests[0];
        assert!(request.starts_with(b"POST /inference HTTP/1.1\r\n"));
        assert!(contains(request, b"name=\"language\"\r\n\r\nfr\r\n"));
        let wav = [b"data".as_slice(), &4u32.to_le_bytes(), &[1, 0, 0xff, 0xff]].concat();
        assert!(contains(request, &wav), "the samples follow the WAV header");
    }

    #[test]
    fn a_server_still_loading_its_model_is_asked_again() {
        let (port, server) = fake_server(vec![
            (503, r#"{"error":"model loading"}"#),
            (200, r#"{"text":"Oui."}"#),
        ]);
        assert_eq!(
            ask(port, "fr", &[0; 16], PATIENCE, || true).as_deref(),
            Some("Oui.")
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn a_server_that_died_is_not_waited_for() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let started = Instant::now();
        assert_eq!(ask(port, "fr", &[0; 16], PATIENCE, || false), None);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn an_empty_answer_leaves_the_stream_s_text() {
        let (port, _) = fake_server(vec![(200, r#"{"text":"  "}"#)]);
        assert_eq!(ask(port, "fr", &[0; 16], PATIENCE, || true), None);
    }
}
