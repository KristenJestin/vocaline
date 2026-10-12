mod audio;
mod cli;
mod config;
mod daemon;
mod engine;
mod events;
mod output;
mod rules;
mod steady;
mod vocabulary;

use audio::{Microphone, PwRecord, WavFile};
use cli::Cli;
use config::{Config, Dirs};
use engine::CrispAsr;
use output::{Hyprland, Output, Paste, Ports, Timing, Wayland, Wtype};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match Cli::parse(&args) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("vocaline: {error}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vocaline: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let dirs = Dirs::from_env()?;
    match cli {
        Cli::Help => println!("{}", cli::USAGE),
        Cli::Daemon { audio_file } => run_daemon(&dirs, audio_file.as_deref())?,
        Cli::Command(command) => {
            println!("{}", cli::request(&dirs.socket(), command.word())?.name());
        }
        Cli::Last => {
            for phrase in cli::last(&dirs.socket())? {
                println!("{phrase}");
            }
        }
        Cli::Teach { heard, wanted } => {
            vocabulary::teach(&dirs.vocabulary_file(), &heard, &wanted)?;
            println!("{} -> {}", heard.trim(), wanted.trim());
        }
        Cli::Watch => cli::watch(&dirs.socket(), &mut std::io::stdout().lock())?,
    }
    Ok(())
}

fn run_daemon(dirs: &Dirs, audio_file: Option<&Path>) -> Result<(), String> {
    let config = Config::load(dirs)?;
    let mic: Arc<dyn Microphone> = match audio_file {
        Some(path) => Arc::new(WavFile::new(path)),
        None => Arc::new(PwRecord),
    };
    let engine = Arc::new(CrispAsr::new(config.engine));
    let socket = dirs.socket();
    let listener = daemon::bind(&socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    eprintln!("vocaline: listening for commands on {}", socket.display());
    let hold = Duration::from_millis(config.key.hold_ms);
    let paste = config.output.paste.then(|| {
        Box::new(Output::spawn(
            Ports {
                desktop: Arc::new(Hyprland),
                clipboard: Arc::new(Wayland),
                keyboard: Arc::new(Wtype),
            },
            Timing::default(),
        )) as Box<dyn Paste>
    });
    daemon::serve(
        listener,
        daemon::spawn(
            mic,
            engine,
            paste,
            hold,
            Arc::new(Instant::now),
            dirs.vocabulary_file(),
        ),
    );
    Ok(())
}
