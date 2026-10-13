//! `vocaline setup`: downloads the model and the VAD model the configuration names, so the
//! package carries none of them. Only files Vocaline knows are fetched: their size and checksum
//! are written here, so a truncated or replaced file is refused instead of loaded.

use crate::config::{Config, Dirs, EngineConfig};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A file Vocaline knows how to download.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Known {
    pub name: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

/// Sizes and checksums as Hugging Face publishes them.
pub const CATALOGUE: &[Known] = &[
    Known {
        name: "parakeet-tdt-0.6b-v3-q8_0.gguf",
        url: "https://huggingface.co/cstr/parakeet-tdt-0.6b-v3-GGUF/resolve/main/parakeet-tdt-0.6b-v3-q8_0.gguf",
        size: 674_342_336,
        sha256: "300de963db10e991a8c3c1674000245546f2e99d396f860aecbde2dd0534e43f",
    },
    Known {
        name: "parakeet-tdt-0.6b-v3-q5_0.gguf",
        url: "https://huggingface.co/cstr/parakeet-tdt-0.6b-v3-GGUF/resolve/main/parakeet-tdt-0.6b-v3-q5_0.gguf",
        size: 470_255_392,
        sha256: "82a602e5f102e9dd11909d8fee1875a28acaa95e7caebc4aa5170b2df2e066fe",
    },
    Known {
        name: "parakeet-tdt-0.6b-v3-q4_k.gguf",
        url: "https://huggingface.co/cstr/parakeet-tdt-0.6b-v3-GGUF/resolve/main/parakeet-tdt-0.6b-v3-q4_k.gguf",
        size: 417_894_912,
        sha256: "addef407ab841eef45739e78920fd8c00e63f78eaa823c490e600e0ab58080a1",
    },
    Known {
        name: "canary-1b-v2-q8_0.gguf",
        url: "https://huggingface.co/cstr/canary-1b-v2-GGUF/resolve/main/canary-1b-v2-q8_0.gguf",
        size: 1_049_050_784,
        sha256: "81421a82cdcb23746f6c3e8c2859094f7e8c27dd8ba77d4b8f8440395fbc7c7a",
    },
    Known {
        name: "ggml-silero-v6.2.0.bin",
        url: "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v6.2.0.bin",
        size: 885_098,
        sha256: "2aa269b785eeb53a82983a20501ddf7c1d9c48e33ab63a41391ac6c9f7fb6987",
    },
];

/// Fetches `url` into `to`; the network behind a trait so the tests never touch it.
pub trait Fetch {
    fn fetch(&self, url: &str, to: &Path) -> Result<(), String>;
}

/// `curl`, which every Arch machine has and which shows its own progress bar.
pub struct Curl;

impl Fetch for Curl {
    fn fetch(&self, url: &str, to: &Path) -> Result<(), String> {
        let status = Command::new("curl")
            .args(["--fail", "--location", "--progress-bar", "--output"])
            .arg(to)
            .arg(url)
            .status()
            .map_err(|e| format!("curl: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("curl could not download {url} ({status})"))
        }
    }
}

/// What tells whether the engine can use an NVIDIA GPU; behind a trait so the tests choose.
pub trait Gpu {
    fn usable(&self) -> bool;
}

/// The block measured in #8 for a machine without a GPU.
pub const CPU_BLOCK: &str = "[engine]\n\
model = \"parakeet-tdt-0.6b-v3-q4_k.gguf\"\n\
backend = \"cpu\"\n\
partial_ms = 1500\n";

/// `nvidia-smi -L`: it lists the cards the driver sees, and loads nothing on them. A missing
/// binary, a driver that is not loaded or a machine without a card all end in "not usable".
pub struct NvidiaSmi;

impl Gpu for NvidiaSmi {
    fn usable(&self) -> bool {
        Command::new("nvidia-smi")
            .arg("-L")
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success() && lists_a_gpu(&String::from_utf8_lossy(&o.stdout)))
    }
}

fn lists_a_gpu(listing: &str) -> bool {
    listing.lines().any(|line| line.starts_with("GPU "))
}

/// Loads the configuration, after setting the CPU up when there is no GPU: a new machine gets
/// the block written, a configuration that exists is never edited, only shown the block.
pub fn configure(dirs: &Dirs, gpu: &dyn Gpu, out: &mut impl Write) -> Result<Config, String> {
    let path = dirs.config_file();
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if gpu.usable() {
        return Config::load(dirs);
    }
    let shown = |e: std::io::Error| e.to_string();
    match existing {
        None => {
            std::fs::create_dir_all(&dirs.config)
                .and_then(|()| std::fs::write(&path, CPU_BLOCK))
                .map_err(|e| format!("{}: {e}", path.display()))?;
            writeln!(
                out,
                "no NVIDIA GPU found: wrote {} for the CPU (q4_k model, 1.5 s between partials)",
                path.display()
            )
            .map_err(shown)?;
        }
        Some(text) => {
            // A broken file is reported by `load`, as it is without this step.
            let config = Config::load(dirs)?;
            if !sets_backend(&text) {
                writeln!(
                    out,
                    "no NVIDIA GPU found and {} does not set `backend`: to run on the CPU, put \
                     this in it:\n\n{CPU_BLOCK}",
                    path.display()
                )
                .map_err(shown)?;
            }
            return Ok(config);
        }
    }
    Config::load(dirs)
}

fn sets_backend(text: &str) -> bool {
    text.parse::<toml::Table>()
        .is_ok_and(|t| t.get("engine").and_then(|e| e.get("backend")).is_some())
}

/// The files the engine needs: the model, and the VAD model unless CrispASR fetches it itself.
pub fn wanted(engine: &EngineConfig) -> Vec<PathBuf> {
    let mut files = vec![engine.model.clone()];
    if engine.vad_model != "auto" {
        files.push(PathBuf::from(&engine.vad_model));
    }
    files
}

/// Downloads each file of `wanted` that is missing, after saying what and how much.
pub fn setup(
    wanted: &[PathBuf],
    catalogue: &[Known],
    fetch: &dyn Fetch,
    out: &mut impl Write,
) -> Result<(), String> {
    let mut missing = Vec::new();
    for path in wanted {
        if path.exists() {
            writeln!(out, "already there: {}", path.display()).map_err(|e| e.to_string())?;
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Every file is checked before anything is downloaded: a typo in the configuration
        // should not cost a gigabyte first.
        let known = catalogue.iter().find(|k| k.name == name).ok_or_else(|| {
            format!(
                "{} is missing, and vocaline does not know where to download it: \
                 put it there yourself",
                path.display()
            )
        })?;
        missing.push((path, known));
    }
    if missing.is_empty() {
        return Ok(());
    }
    let total: u64 = missing.iter().map(|(_, k)| k.size).sum();
    for (path, known) in &missing {
        writeln!(
            out,
            "to download: {} ({})",
            path.display(),
            size(known.size)
        )
        .map_err(|e| e.to_string())?;
    }
    writeln!(out, "total: {}", size(total)).map_err(|e| e.to_string())?;
    for (path, known) in missing {
        download(path, known, fetch)?;
        writeln!(out, "downloaded: {}", path.display()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Downloads beside the target and renames only a verified file: an interrupted or corrupt
/// download never takes the place of the model.
fn download(path: &Path, known: &Known, fetch: &dyn Fetch) -> Result<(), String> {
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    }
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let checked = fetch
        .fetch(known.url, &part)
        .and_then(|()| check(&part, known));
    match checked {
        Ok(()) => std::fs::rename(&part, path).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

fn check(path: &Path, known: &Known) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    let mut read = 0;
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        read += n as u64;
    }
    let sha256: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if read == known.size && sha256 == known.sha256 {
        Ok(())
    } else {
        Err(format!(
            "{} does not match what was published ({read} bytes, sha256 {sha256}); \
             nothing was kept",
            known.name
        ))
    }
}

fn size(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Backend;
    use std::cell::RefCell;

    /// A network that serves `body` for every URL and remembers what it was asked.
    struct FakeNetwork {
        body: &'static [u8],
        asked: RefCell<Vec<String>>,
    }

    impl FakeNetwork {
        fn serving(body: &'static [u8]) -> FakeNetwork {
            FakeNetwork {
                body,
                asked: RefCell::new(Vec::new()),
            }
        }
    }

    impl Fetch for FakeNetwork {
        fn fetch(&self, url: &str, to: &Path) -> Result<(), String> {
            self.asked.borrow_mut().push(url.to_string());
            std::fs::write(to, self.body).map_err(|e| e.to_string())
        }
    }

    const HELLO: Known = Known {
        name: "model.gguf",
        url: "https://example.invalid/model.gguf",
        size: 5,
        // sha256 of "hello"
        sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
    };

    fn folder(name: &str) -> PathBuf {
        let folder =
            std::env::temp_dir().join(format!("vocaline-setup-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        folder
    }

    #[test]
    fn setup_shows_the_sizes_then_downloads_what_is_missing() {
        let path = folder("missing").join("models/model.gguf");
        let network = FakeNetwork::serving(b"hello");
        let mut out = Vec::new();
        setup(std::slice::from_ref(&path), &[HELLO], &network, &mut out).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert_eq!(*network.asked.borrow(), [HELLO.url]);
        let out = String::from_utf8(out).unwrap();
        let shown = out.find("to download: ").unwrap();
        let done = out.find("downloaded: ").unwrap();
        assert!(shown < done, "{out}");
        assert!(
            out.contains("(0.0 MB)") && out.contains("total: 0.0 MB"),
            "{out}"
        );
    }

    #[test]
    fn setup_does_nothing_when_the_files_are_already_there() {
        let path = folder("there").join("model.gguf");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"mine").unwrap();
        let network = FakeNetwork::serving(b"hello");
        let mut out = Vec::new();
        setup(std::slice::from_ref(&path), &[HELLO], &network, &mut out).unwrap();
        assert!(network.asked.borrow().is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), b"mine");
        assert!(String::from_utf8(out).unwrap().contains("already there"));
    }

    #[test]
    fn a_wrong_checksum_keeps_nothing_and_is_an_error() {
        let folder = folder("corrupt");
        let path = folder.join("model.gguf");
        let network = FakeNetwork::serving(b"hellp");
        let error = setup(
            std::slice::from_ref(&path),
            &[HELLO],
            &network,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.contains("does not match"), "{error}");
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 0);
    }

    #[test]
    fn an_unknown_file_is_an_error_before_anything_is_downloaded() {
        let folder = folder("unknown");
        let wanted = [folder.join("model.gguf"), folder.join("mine.gguf")];
        let network = FakeNetwork::serving(b"hello");
        let error = setup(&wanted, &[HELLO], &network, &mut Vec::new()).unwrap_err();
        assert!(error.contains("mine.gguf"), "{error}");
        assert!(network.asked.borrow().is_empty());
    }

    #[test]
    fn setup_wants_the_model_and_the_vad_model_but_not_an_auto_vad() {
        let engine = EngineConfig {
            model: "/m/model.gguf".into(),
            vad_model: "/m/vad.bin".into(),
            ..EngineConfig::default()
        };
        assert_eq!(
            wanted(&engine),
            [PathBuf::from("/m/model.gguf"), PathBuf::from("/m/vad.bin")]
        );
        let engine = EngineConfig {
            vad_model: "auto".into(),
            ..engine
        };
        assert_eq!(wanted(&engine), [PathBuf::from("/m/model.gguf")]);
    }

    #[test]
    fn the_default_configuration_is_in_the_catalogue() {
        let engine = EngineConfig::default();
        for name in [&engine.model, &PathBuf::from(&engine.vad_model)] {
            let name = name.to_str().unwrap();
            let known = CATALOGUE.iter().find(|k| k.name == name);
            assert!(known.is_some_and(|k| k.url.ends_with(name)), "{name}");
        }
    }

    struct FakeGpu(bool);

    impl Gpu for FakeGpu {
        fn usable(&self) -> bool {
            self.0
        }
    }

    fn dirs_in(name: &str) -> Dirs {
        let root = folder(name);
        Dirs {
            config: root.join("config/vocaline"),
            data: root.join("data/vocaline"),
            runtime: root.join("run"),
        }
    }

    fn written(dirs: &Dirs) -> Option<String> {
        std::fs::read_to_string(dirs.config_file()).ok()
    }

    #[test]
    fn no_gpu_and_no_config_writes_the_cpu_block_and_wants_the_q4_k_model() {
        let dirs = dirs_in("nogpu-new");
        let mut out = Vec::new();
        let config = configure(&dirs, &FakeGpu(false), &mut out).unwrap();
        assert_eq!(written(&dirs).as_deref(), Some(CPU_BLOCK));
        assert_eq!(config.engine.backend, Backend::Cpu);
        assert_eq!(config.engine.partial_ms, 1500);
        let models = dirs.data.join("models");
        assert_eq!(
            wanted(&config.engine),
            [
                models.join("parakeet-tdt-0.6b-v3-q4_k.gguf"),
                models.join("ggml-silero-v6.2.0.bin")
            ]
        );
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(
            out.contains(&dirs.config_file().display().to_string()),
            "{out}"
        );
    }

    #[test]
    fn no_gpu_and_an_existing_config_leaves_it_byte_for_byte_and_prints_the_block() {
        let dirs = dirs_in("nogpu-existing");
        std::fs::create_dir_all(&dirs.config).unwrap();
        let mine = "# mine\n[engine]\nlanguage = \"en\"\n\n[key]\nhold_ms = 400";
        std::fs::write(dirs.config_file(), mine).unwrap();
        let mut out = Vec::new();
        let config = configure(&dirs, &FakeGpu(false), &mut out).unwrap();
        assert_eq!(written(&dirs).as_deref(), Some(mine));
        assert_eq!(config.engine.backend, Backend::Cuda);
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(CPU_BLOCK), "{out}");
        assert!(out.contains("no NVIDIA GPU"), "{out}");
    }

    #[test]
    fn no_gpu_and_a_config_that_sets_the_backend_is_left_alone_in_silence() {
        let dirs = dirs_in("nogpu-backend");
        std::fs::create_dir_all(&dirs.config).unwrap();
        let mine = "[engine]\nbackend = \"vulkan\"\n";
        std::fs::write(dirs.config_file(), mine).unwrap();
        let mut out = Vec::new();
        let config = configure(&dirs, &FakeGpu(false), &mut out).unwrap();
        assert_eq!(written(&dirs).as_deref(), Some(mine));
        assert_eq!(config.engine.backend, Backend::Vulkan);
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn a_gpu_changes_nothing() {
        let dirs = dirs_in("gpu");
        let mut out = Vec::new();
        let config = configure(&dirs, &FakeGpu(true), &mut out).unwrap();
        assert_eq!(written(&dirs), None);
        assert_eq!(config, Config::load(&dirs).unwrap());
        assert_eq!(config.engine.backend, Backend::Cuda);
        assert!(out.is_empty());
    }

    #[test]
    fn a_broken_config_is_still_an_error_without_a_gpu() {
        let dirs = dirs_in("nogpu-broken");
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(dirs.config_file(), "[engine\n").unwrap();
        let error = configure(&dirs, &FakeGpu(false), &mut Vec::new()).unwrap_err();
        assert!(error.contains("config.toml"), "{error}");
        assert_eq!(written(&dirs).as_deref(), Some("[engine\n"));
    }

    #[test]
    fn nvidia_smi_lists_a_gpu_or_it_does_not() {
        assert!(lists_a_gpu(
            "GPU 0: NVIDIA GeForce RTX 3060 (UUID: GPU-1234)\n"
        ));
        assert!(!lists_a_gpu(""));
        assert!(!lists_a_gpu("No devices were found\n"));
    }
}
