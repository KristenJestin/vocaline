//! What differs between machines: the engine binary, the model, the backend, the cadence.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The engine binary when the configuration names none. The Arch package bundles CrispASR in
/// its own folder and sets `VOCALINE_ENGINE` at build time, so it never clashes with another
/// `crispasr` in `PATH`; a plain `cargo build` looks it up in `PATH`.
pub const DEFAULT_ENGINE: &str = match option_env!("VOCALINE_ENGINE") {
    Some(path) => path,
    None => "crispasr",
};

/// The folders Vocaline uses, from the XDG variables.
#[derive(Debug, Clone, PartialEq)]
pub struct Dirs {
    /// `$XDG_CONFIG_HOME/vocaline`: holds `config.toml` and `vocabulary.txt`.
    pub config: PathBuf,
    /// `$XDG_DATA_HOME/vocaline`: a relative model path is read from its `models` folder.
    pub data: PathBuf,
    /// `$XDG_RUNTIME_DIR`: holds the control socket.
    pub runtime: PathBuf,
}

impl Dirs {
    pub fn from_env() -> Result<Dirs, String> {
        Dirs::from_vars(|name| std::env::var_os(name).map(PathBuf::from))
    }

    fn from_vars(var: impl Fn(&str) -> Option<PathBuf>) -> Result<Dirs, String> {
        // The XDG specification says a relative value is invalid and must be ignored.
        let var = |name: &str| var(name).filter(|p| p.is_absolute());
        let home = var("HOME");
        let base = |name: &str, fallback: &str| {
            var(name)
                .or_else(|| home.as_ref().map(|h| h.join(fallback)))
                .ok_or_else(|| format!("neither {name} nor HOME is set"))
        };
        Ok(Dirs {
            config: base("XDG_CONFIG_HOME", ".config")?.join("vocaline"),
            data: base("XDG_DATA_HOME", ".local/share")?.join("vocaline"),
            runtime: var("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?,
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    pub fn vocabulary_file(&self) -> PathBuf {
        self.config.join("vocabulary.txt")
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime.join("vocaline.sock")
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub engine: EngineConfig,
    pub key: KeyConfig,
    pub output: OutputConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    /// Paste each final phrase where the dictation started; off, the phrases are only in the
    /// event stream (for a voice assistant).
    pub paste: bool,
}

impl Default for OutputConfig {
    fn default() -> OutputConfig {
        OutputConfig { paste: true }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeyConfig {
    /// A key held at least this long stops the dictation on release; a shorter press is a tap
    /// that leaves it listening.
    pub hold_ms: u64,
}

impl Default for KeyConfig {
    fn default() -> KeyConfig {
        KeyConfig { hold_ms: 250 }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    /// The `crispasr` binary: a path, or a name looked up in `PATH`.
    pub command: PathBuf,
    /// The GGUF model, Parakeet or Canary: crispasr knows which from the file. A relative path
    /// is read from `$XDG_DATA_HOME/vocaline/models`.
    pub model: PathBuf,
    /// The Silero VAD model, read like `model`; `auto` lets CrispASR fetch and cache it itself.
    pub vad_model: String,
    pub backend: Backend,
    pub language: String,
    /// How often the open utterance is decoded again (a partial).
    pub partial_ms: u32,
    /// The silence that ends a phrase (a final).
    pub final_silence_ms: u32,
}

impl Default for EngineConfig {
    fn default() -> EngineConfig {
        EngineConfig {
            command: PathBuf::from(DEFAULT_ENGINE),
            model: PathBuf::from("parakeet-tdt-0.6b-v3-q8_0.gguf"),
            vad_model: "ggml-silero-v6.2.0.bin".into(),
            backend: Backend::Cuda,
            language: "fr".into(),
            partial_ms: 400,
            final_silence_ms: 700,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Cuda,
    Vulkan,
    Cpu,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Cuda => "cuda",
            Backend::Vulkan => "vulkan",
            Backend::Cpu => "cpu",
        }
    }
}

impl Config {
    /// Reads the configuration file of `dirs`; a missing file means the defaults.
    pub fn load(dirs: &Dirs) -> Result<Config, String> {
        let path = dirs.config_file();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Config::parse(&text, dirs).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn parse(text: &str, dirs: &Dirs) -> Result<Config, String> {
        let mut config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        let models = dirs.data.join("models");
        config.engine.model = models.join(expand_home(&config.engine.model));
        if config.engine.vad_model != "auto" {
            let vad = models.join(expand_home(Path::new(&config.engine.vad_model)));
            config.engine.vad_model = vad.to_string_lossy().into_owned();
        }
        config.engine.command = expand_home(&config.engine.command);
        Ok(config)
    }
}

/// `~/x` is what people write in a configuration file; the shell is not there to expand it.
fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> Dirs {
        Dirs {
            config: "/cfg/vocaline".into(),
            data: "/data/vocaline".into(),
            runtime: "/run/user/1".into(),
        }
    }

    #[test]
    fn defaults_are_the_values_of_the_issue() {
        let config = Config::parse("", &dirs()).unwrap();
        let engine = config.engine;
        assert_eq!(engine.command, PathBuf::from(DEFAULT_ENGINE));
        assert_eq!(
            engine.model,
            PathBuf::from("/data/vocaline/models/parakeet-tdt-0.6b-v3-q8_0.gguf")
        );
        assert_eq!(
            engine.vad_model,
            "/data/vocaline/models/ggml-silero-v6.2.0.bin"
        );
        assert_eq!(engine.backend, Backend::Cuda);
        assert_eq!(engine.language, "fr");
        assert_eq!(engine.partial_ms, 400);
        assert_eq!(engine.final_silence_ms, 700);
    }

    #[test]
    fn the_key_holds_for_250_ms_by_default_and_the_file_can_change_it() {
        assert_eq!(Config::parse("", &dirs()).unwrap().key.hold_ms, 250);
        let config = Config::parse("[key]\nhold_ms = 400", &dirs()).unwrap();
        assert_eq!(config.key.hold_ms, 400);
    }

    #[test]
    fn pasting_is_on_by_default_and_the_file_can_turn_it_off() {
        assert!(Config::parse("", &dirs()).unwrap().output.paste);
        let config = Config::parse("[output]\npaste = false", &dirs()).unwrap();
        assert!(!config.output.paste);
    }

    #[test]
    fn the_file_overrides_what_differs_between_machines() {
        let text = r#"
            [engine]
            command = "/opt/crispasr/crispasr"
            model = "/models/parakeet-q4_k.gguf"
            backend = "cpu"
            language = "en"
            partial_ms = 800
            final_silence_ms = 1000
        "#;
        let engine = Config::parse(text, &dirs()).unwrap().engine;
        assert_eq!(engine.command, PathBuf::from("/opt/crispasr/crispasr"));
        assert_eq!(engine.model, PathBuf::from("/models/parakeet-q4_k.gguf"));
        assert_eq!(engine.backend, Backend::Cpu);
        assert_eq!(engine.language, "en");
        assert_eq!(engine.partial_ms, 800);
        assert_eq!(engine.final_silence_ms, 1000);
    }

    #[test]
    fn a_relative_model_is_read_from_the_data_folder() {
        let text = "[engine]\nmodel = \"other.gguf\"";
        let engine = Config::parse(text, &dirs()).unwrap().engine;
        assert_eq!(
            engine.model,
            PathBuf::from("/data/vocaline/models/other.gguf")
        );
    }

    #[test]
    fn a_relative_vad_model_is_read_from_the_data_folder_and_auto_is_left_to_crispasr() {
        let text = "[engine]\nvad_model = \"silero.bin\"";
        let engine = Config::parse(text, &dirs()).unwrap().engine;
        assert_eq!(engine.vad_model, "/data/vocaline/models/silero.bin");
        let text = "[engine]\nvad_model = \"auto\"";
        assert_eq!(
            Config::parse(text, &dirs()).unwrap().engine.vad_model,
            "auto"
        );
    }

    #[test]
    fn a_build_without_a_bundled_engine_looks_crispasr_up_in_path() {
        if option_env!("VOCALINE_ENGINE").is_none() {
            assert_eq!(DEFAULT_ENGINE, "crispasr");
        }
    }

    #[test]
    fn an_unknown_key_is_an_error_not_a_silent_default() {
        let error = Config::parse("[engine]\npartial = 400", &dirs()).unwrap_err();
        assert!(error.contains("partial"), "{error}");
    }

    #[test]
    fn an_unknown_backend_is_an_error() {
        assert!(Config::parse("[engine]\nbackend = \"metal\"", &dirs()).is_err());
    }

    #[test]
    fn a_missing_file_means_the_defaults() {
        let folder = std::env::temp_dir().join(format!("vocaline-config-{}", std::process::id()));
        let dirs = Dirs {
            config: folder.join("vocaline"),
            ..dirs()
        };
        assert_eq!(
            Config::load(&dirs).unwrap(),
            Config::parse("", &dirs).unwrap()
        );
    }

    #[test]
    fn the_folders_follow_xdg_and_fall_back_on_home() {
        let vars = |name: &str| match name {
            "HOME" => Some(PathBuf::from("/home/u")),
            "XDG_RUNTIME_DIR" => Some(PathBuf::from("/run/user/1")),
            "XDG_CONFIG_HOME" => Some(PathBuf::from("/tmp/cfg")),
            "XDG_DATA_HOME" => Some(PathBuf::from("relative/ignored")),
            _ => None,
        };
        let dirs = Dirs::from_vars(vars).unwrap();
        assert_eq!(
            dirs.config_file(),
            PathBuf::from("/tmp/cfg/vocaline/config.toml")
        );
        assert_eq!(dirs.data, PathBuf::from("/home/u/.local/share/vocaline"));
        assert_eq!(dirs.socket(), PathBuf::from("/run/user/1/vocaline.sock"));
    }

    #[test]
    fn no_runtime_folder_is_an_error() {
        let vars = |name: &str| (name == "HOME").then(|| PathBuf::from("/home/u"));
        assert!(Dirs::from_vars(vars).is_err());
    }
}
