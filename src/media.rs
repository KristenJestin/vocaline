//! The media players, paused while a dictation runs so they do not play over the voice and into
//! the microphone.
//!
//! The session bus is a port, so the logic is tested with a fake. The bus is slow to answer or
//! not there at all more often than the other ports: the calls run on their own thread, fed in
//! order, and the daemon never waits for them.

use crate::config::MediaConfig;
use std::io;
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::thread;

/// How long the bus is given to answer one call: a player that hangs must not hold the others.
const CALL_TIMEOUT_SECS: &str = "2";

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER: &str = "org.mpris.MediaPlayer2.Player";

/// The media players of the session, told apart by their name on the bus.
pub trait Media: Send + Sync {
    /// The players that are playing.
    fn playing(&self) -> io::Result<Vec<String>>;
    fn is_paused(&self, player: &str) -> io::Result<bool>;
    fn pause(&self, player: &str) -> io::Result<()>;
    fn play(&self, player: &str) -> io::Result<()>;
}

enum Job {
    Pause,
    Resume,
    #[cfg(test)]
    Settle(mpsc::Sender<()>),
}

/// Pauses the players at the start of a dictation and plays again the ones it paused.
#[derive(Clone)]
pub struct Pauser {
    jobs: mpsc::Sender<Job>,
}

/// A media player that cannot be reached is a warning: the dictation goes on without it.
fn warn(what: &str, player: &str, error: &io::Error) {
    eprintln!("vocaline: could not {what} the media player {player}: {error}");
}

/// Pauses the players that are playing, and returns the ones it paused.
fn pause_playing(media: &dyn Media) -> Vec<String> {
    let playing = match media.playing() {
        Ok(playing) => playing,
        Err(e) => {
            eprintln!("vocaline: could not list the media players: {e}");
            return Vec::new();
        }
    };
    playing
        .into_iter()
        .filter(|player| match media.pause(player) {
            Ok(()) => true,
            Err(e) => {
                warn("pause", player, &e);
                false
            }
        })
        .collect()
}

/// Plays again the `paused` players that are still paused: one played by hand since, or closed,
/// is not touched.
fn play_again(media: &dyn Media, paused: Vec<String>) {
    for player in paused {
        match media.is_paused(&player) {
            Ok(true) => {
                if let Err(e) = media.play(&player) {
                    warn("play", &player, &e);
                }
            }
            Ok(false) => {}
            Err(e) => warn("ask", &player, &e),
        }
    }
}

impl Pauser {
    /// The pauser the configuration asks for, if it asks for one.
    pub fn for_config(config: &MediaConfig, media: Arc<dyn Media>) -> Option<Pauser> {
        config.pause.then(|| Pauser::spawn(media))
    }

    pub fn spawn(media: Arc<dyn Media>) -> Pauser {
        let (jobs, inbox) = mpsc::channel();
        thread::spawn(move || {
            // The players paused for the dictation going on, none between two dictations.
            let mut paused: Vec<String> = Vec::new();
            for job in inbox {
                match job {
                    Job::Pause => paused = pause_playing(&*media),
                    Job::Resume => play_again(&*media, std::mem::take(&mut paused)),
                    #[cfg(test)]
                    Job::Settle(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        Pauser { jobs }
    }

    /// Pauses the players that are playing, and remembers which.
    pub fn pause(&self) {
        let _ = self.jobs.send(Job::Pause);
    }

    /// Plays again the players paused, if they are still paused.
    pub fn resume(&self) {
        let _ = self.jobs.send(Job::Resume);
    }

    /// Returns once everything asked so far is done.
    #[cfg(test)]
    pub fn settle(&self) {
        let (done, wait) = mpsc::channel();
        self.jobs.send(Job::Settle(done)).unwrap();
        wait.recv().unwrap();
    }
}

/// The players of the session bus, through `busctl` (part of systemd, so on every machine that
/// has a session bus): a library for the bus would weigh far more than the three calls made here.
pub struct Mpris;

fn busctl(args: &[&str]) -> io::Result<Vec<u8>> {
    let output = Command::new("busctl")
        .args(["--user", "--json=short", "--timeout", CALL_TIMEOUT_SECS])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "busctl {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn player_call(player: &str, method: &str) -> io::Result<Vec<u8>> {
    busctl(&["call", player, MPRIS_PATH, MPRIS_PLAYER, method])
}

impl Mpris {
    /// The arguments of the listing. Without `--acquired` it also names the services the bus
    /// could start: asking one for its status would launch a player that is not running.
    fn list_args() -> [&'static str; 2] {
        ["list", "--acquired"]
    }

    /// The names of the media players on the bus, from `busctl list`.
    fn parse_players(json: &[u8]) -> io::Result<Vec<String>> {
        let names: serde_json::Value = serde_json::from_slice(json)?;
        Ok(names
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["name"].as_str())
            .filter(|name| name.starts_with(MPRIS_PREFIX))
            .map(String::from)
            .collect())
    }

    /// `Playing`, `Paused` or `Stopped`, from `busctl get-property PlaybackStatus`.
    fn parse_status(json: &[u8]) -> io::Result<String> {
        let property: serde_json::Value = serde_json::from_slice(json)?;
        property["data"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| io::Error::other("no playback status"))
    }

    fn status(player: &str) -> io::Result<String> {
        Mpris::parse_status(&busctl(&[
            "get-property",
            player,
            MPRIS_PATH,
            MPRIS_PLAYER,
            "PlaybackStatus",
        ])?)
    }
}

impl Media for Mpris {
    fn playing(&self) -> io::Result<Vec<String>> {
        let mut playing = Vec::new();
        for player in Mpris::parse_players(&busctl(&Mpris::list_args())?)? {
            // A player that is gone since the list, or hangs, does not hide the others.
            match Mpris::status(&player) {
                Ok(status) if status == "Playing" => playing.push(player),
                Ok(_) => {}
                Err(e) => warn("ask", &player, &e),
            }
        }
        Ok(playing)
    }

    fn is_paused(&self, player: &str) -> io::Result<bool> {
        Ok(Mpris::status(player)? == "Paused")
    }

    fn pause(&self, player: &str) -> io::Result<()> {
        player_call(player, "Pause").map(drop)
    }

    fn play(&self, player: &str) -> io::Result<()> {
        player_call(player, "Play").map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_players_are_the_mpris_names_of_the_bus_list() {
        let list = br#"[{"name":":1.1","pid":1},
            {"name":"org.mpris.MediaPlayer2.firefox.instance_1_2641","pid":2},
            {"name":"org.freedesktop.Notifications","pid":3},
            {"name":"org.mpris.MediaPlayer2.mpv","pid":4}]"#;
        assert_eq!(
            Mpris::parse_players(list).unwrap(),
            [
                "org.mpris.MediaPlayer2.firefox.instance_1_2641",
                "org.mpris.MediaPlayer2.mpv"
            ]
        );
        assert!(Mpris::parse_players(b"not json").is_err());
    }

    #[test]
    fn only_the_names_a_process_holds_are_listed() {
        assert!(Mpris::list_args().contains(&"--acquired"));
    }

    #[test]
    fn the_playback_status_is_read_from_the_property() {
        let property = br#"{"type":"s","data":"Playing"}"#;
        assert_eq!(Mpris::parse_status(property).unwrap(), "Playing");
        assert!(Mpris::parse_status(b"{}").is_err());
    }
}
