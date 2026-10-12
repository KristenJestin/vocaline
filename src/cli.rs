//! The command line: the daemon, and the commands that talk to it through its socket.

use crate::daemon::{Answer, Command};
use crate::events::State;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

pub const USAGE: &str = "\
usage: vocaline <command>

  daemon [--audio-file <wav>]  own the microphone and the engine; with --audio-file, each
                               dictation plays that 16 kHz mono recording instead
  press                        the key went down: stop when listening, otherwise start
  release                      the key went up: stop if it was held long enough (hold-to-talk)
  start [--no-paste]           start listening; with --no-paste, the final phrases are only
                               in the event stream, nothing is pasted
  stop                         stop listening; the open phrase is finalised
  toggle                       start, or stop
  status                       print the state: idle, loading, listening or stopping
  watch                        print the event stream, one JSON object per line
  last                         print the last final phrases of this session, one per line
  teach \"<heard>\" \"<wanted>\"  add an entry to the vocabulary: what the model writes, and
                               what you want instead (read from the next dictation on)
  setup                        download the model and the VAD model the configuration names,
                               if they are missing; their size is shown first";

#[derive(Debug, PartialEq)]
pub enum Cli {
    Daemon { audio_file: Option<PathBuf> },
    Command(Command),
    Watch,
    Last,
    Teach { heard: String, wanted: String },
    Setup,
    Help,
}

impl Cli {
    pub fn parse(args: &[String]) -> Result<Cli, String> {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["daemon"] => Ok(Cli::Daemon { audio_file: None }),
            ["daemon", "--audio-file", path] => Ok(Cli::Daemon {
                audio_file: Some(PathBuf::from(path)),
            }),
            ["start", "--no-paste"] => Ok(Cli::Command(Command::StartNoPaste)),
            ["watch"] => Ok(Cli::Watch),
            ["last"] => Ok(Cli::Last),
            ["teach", heard, wanted] => Ok(Cli::Teach {
                heard: heard.to_string(),
                wanted: wanted.to_string(),
            }),
            ["setup"] => Ok(Cli::Setup),
            ["help" | "--help" | "-h"] => Ok(Cli::Help),
            [word] => Command::parse(word)
                .map(Cli::Command)
                .ok_or_else(|| format!("unknown command: {word}")),
            _ => Err("expected one command".into()),
        }
    }
}

/// Sends `command` to the daemon and returns the state it reached.
pub fn request(socket: &Path, command: &str) -> Result<State, String> {
    match ask(socket, command)? {
        Answer {
            state: Some(state), ..
        } => Ok(state),
        Answer {
            error: Some(error), ..
        } => Err(error),
        answer => Err(format!("unexpected answer from the daemon: {answer:?}")),
    }
}

/// The last final phrases of the daemon's session, oldest first.
pub fn last(socket: &Path) -> Result<Vec<String>, String> {
    match ask(socket, "last")? {
        Answer {
            phrases: Some(phrases),
            ..
        } => Ok(phrases),
        Answer {
            error: Some(error), ..
        } => Err(error),
        answer => Err(format!("unexpected answer from the daemon: {answer:?}")),
    }
}

fn ask(socket: &Path, command: &str) -> Result<Answer, String> {
    let mut stream = connect(socket)?;
    writeln!(stream, "{command}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|_| format!("unexpected answer from the daemon: {line:?}"))
}

/// Copies the event stream to `out` until the daemon goes away.
pub fn watch(socket: &Path, out: &mut impl Write) -> Result<(), String> {
    let mut stream = connect(socket)?;
    writeln!(stream, "watch").map_err(|e| e.to_string())?;
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|e| e.to_string())?;
        // A reader on the other side of a pipe wants each event as it happens.
        writeln!(out, "{line}")
            .and_then(|()| out.flush())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn connect(socket: &Path) -> Result<UnixStream, String> {
    UnixStream::connect(socket).map_err(|e| {
        format!(
            "no daemon on {} ({e}); start it with `vocaline daemon`",
            socket.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn each_command_is_read_from_its_word() {
        assert_eq!(
            Cli::parse(&args(&["daemon"])),
            Ok(Cli::Daemon { audio_file: None })
        );
        assert_eq!(
            Cli::parse(&args(&["daemon", "--audio-file", "a.wav"])),
            Ok(Cli::Daemon {
                audio_file: Some("a.wav".into())
            })
        );
        assert_eq!(
            Cli::parse(&args(&["start"])),
            Ok(Cli::Command(Command::Start))
        );
        assert_eq!(
            Cli::parse(&args(&["stop"])),
            Ok(Cli::Command(Command::Stop))
        );
        assert_eq!(
            Cli::parse(&args(&["toggle"])),
            Ok(Cli::Command(Command::Toggle))
        );
        assert_eq!(
            Cli::parse(&args(&["press"])),
            Ok(Cli::Command(Command::Press))
        );
        assert_eq!(
            Cli::parse(&args(&["release"])),
            Ok(Cli::Command(Command::Release))
        );
        assert_eq!(
            Cli::parse(&args(&["status"])),
            Ok(Cli::Command(Command::Status))
        );
        assert_eq!(
            Cli::parse(&args(&["start", "--no-paste"])),
            Ok(Cli::Command(Command::StartNoPaste))
        );
        assert_eq!(Cli::parse(&args(&["watch"])), Ok(Cli::Watch));
        assert_eq!(Cli::parse(&args(&["setup"])), Ok(Cli::Setup));
        assert_eq!(Cli::parse(&args(&["--help"])), Ok(Cli::Help));
        assert_eq!(Cli::parse(&args(&["last"])), Ok(Cli::Last));
        assert_eq!(
            Cli::parse(&args(&["teach", "foo bar", "Foobar"])),
            Ok(Cli::Teach {
                heard: "foo bar".into(),
                wanted: "Foobar".into()
            })
        );
    }

    #[test]
    fn a_wrong_command_line_is_an_error() {
        assert!(Cli::parse(&args(&[])).is_err());
        assert!(Cli::parse(&args(&["dance"])).is_err());
        assert!(Cli::parse(&args(&["start", "now"])).is_err());
        assert!(Cli::parse(&args(&["daemon", "--audio-file"])).is_err());
        assert!(Cli::parse(&args(&["last", "now"])).is_err());
        assert!(Cli::parse(&args(&["teach"])).is_err());
        assert!(Cli::parse(&args(&["teach", "foo bar"])).is_err());
        assert!(Cli::parse(&args(&["teach", "foo", "bar", "baz"])).is_err());
    }

    /// A daemon that answers one connection with `answer` and returns what it was asked.
    fn one_answer(name: &str, answer: &'static str) -> (PathBuf, thread::JoinHandle<String>) {
        let folder = std::env::temp_dir().join(format!("vocaline-cli-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join(name);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let asked = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            stream.write_all(answer.as_bytes()).unwrap();
            line
        });
        (path, asked)
    }

    #[test]
    fn a_command_returns_the_state_the_daemon_reached() {
        let (path, asked) = one_answer("state.sock", "{\"state\":\"loading\"}\n");
        assert_eq!(request(&path, "start"), Ok(State::Loading));
        assert_eq!(asked.join().unwrap(), "start\n");
    }

    #[test]
    fn a_refused_command_is_an_error_with_the_daemon_reason() {
        let (path, _) = one_answer("refused.sock", "{\"error\":\"still stopping\"}\n");
        assert_eq!(request(&path, "start"), Err("still stopping".into()));
    }

    #[test]
    fn no_daemon_is_an_error_that_says_how_to_start_it() {
        let error = request(Path::new("/nonexistent/vocaline.sock"), "status").unwrap_err();
        assert!(error.contains("vocaline daemon"), "{error}");
    }

    #[test]
    fn last_returns_the_phrases_the_daemon_kept() {
        let (path, asked) = one_answer("last.sock", "{\"phrases\":[\"Bonjour.\",\"Ça va ?\"]}\n");
        assert_eq!(last(&path), Ok(vec!["Bonjour.".into(), "Ça va ?".into()]));
        assert_eq!(asked.join().unwrap(), "last\n");
    }

    #[test]
    fn last_is_an_error_when_the_daemon_does_not_answer_with_phrases() {
        let (path, _) = one_answer("last-state.sock", "{\"state\":\"idle\"}\n");
        assert!(last(&path).is_err());
    }

    #[test]
    fn watch_copies_the_stream_line_by_line() {
        let lines = "{\"type\":\"hello\",\"version\":1}\n{\"type\":\"state\",\"state\":\"idle\"}\n";
        let (path, asked) = one_answer("watch.sock", lines);
        let mut out = Vec::new();
        watch(&path, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), lines);
        assert_eq!(asked.join().unwrap(), "watch\n");
    }
}
