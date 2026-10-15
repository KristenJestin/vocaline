# Installing Vocaline

How to install Vocaline, choose its model and build it yourself. Back to the [README](../README.md).

## The Arch Linux package

CI builds the package: the `dev-latest` pre-release follows the integration branch, each `v*`
release a version. A release from the tag `vX.Y.Z` is version `X.Y.Z` (the tag must match
`Cargo.toml`); any other build is `X.Y.Z.rN.gHASH`, N commits after the last release tag
(`0.0.0` before the first), so pacman sees a dev build as newer than the release it follows
and older than the next one. `vocaline --version` prints the same string.

```sh
gh release download dev-latest -R KristenJestin/vocaline -p '*.pkg.tar.zst'   # or a v* tag
sudo pacman -U vocaline-*.pkg.tar.zst
```

It installs `vocaline`, a systemd user unit, this documentation, and the engine under
`/usr/lib/vocaline/crispasr`: [CrispASR](https://github.com/CrispStrobe/CrispASR) 0.8.42 (MIT),
built from its source with Vocaline's patch (below), with the CUDA backend and the CPU backends,
which it loads at run time: the same binary runs on a machine without CUDA. For the `cuda`
backend, install `cuda` too. The `vulkan` backend is not in it: point `command` at CrispASR's
Vulkan bundle (below).
Removing the package leaves only your configuration and your models.

Then, as yourself:

```sh
vocaline setup                                   # downloads the models, their size shown first
systemctl --user enable --now vocaline.service   # the daemon, with the graphical session
```

`vocaline setup` downloads the model and the VAD model the configuration names into
`~/.local/share/vocaline/models`, checks their sha256, and leaves alone a file already there:
Parakeet TDT 0.6B v3 q8_0 (674 MB) and Silero VAD v6.2.0 (0.9 MB) by default. It knows
`parakeet-tdt-0.6b-v3-q8_0.gguf`, `-q5_0`, `-q4_k` (for a smaller machine) and
`canary-1b-v2-q8_0.gguf`; another file, put it in the models folder yourself.

Without an NVIDIA GPU (`nvidia-smi -L` lists none), `vocaline setup` sets the engine up for the
CPU. With no `config.toml` yet it writes this block, says so, and downloads the q4_k model:

```toml
[engine]
model = "parakeet-tdt-0.6b-v3-q4_k.gguf"
backend = "cpu"
partial_ms = 1500
```

On the CPU it takes about half the CPU of the default and keeps up with a long phrase; the
first grey text comes about a second later (1.7 s instead of 0.7 s). On a machine without a
GPU, `partial_decode_ms` goes further: the grey text is decoded on its own cadence while the
step stays short, so a phrase still ends as soon as the pause is heard. `0` decodes no grey text
at all (the engine still emits about one just before each final). A `config.toml` that
exists is never edited: if it does not set `backend`, setup prints the block for you to copy.

The unit is bound to `graphical-session.target`, which Hyprland started through `uwsm`
reaches. The daemon pastes through Wayland and asks Hyprland which window has the focus: the
user manager must have `WAYLAND_DISPLAY` and `HYPRLAND_INSTANCE_SIGNATURE` in its environment
(`uwsm` does it; otherwise `systemctl --user import-environment WAYLAND_DISPLAY
HYPRLAND_INSTANCE_SIGNATURE` at login).

## Choosing a model

Parakeet guesses the language by itself and ignores `language`: a short phrase can come out
in English (`allô` → `Hello?`). The package's engine cuts most of it (below), but not all.
**Canary 1B v2** obeys `language`, at a cost: on the same GPU
it takes about 3.5 times the CPU while you speak, 0.4 GB more VRAM and 0.15 s more before each
final. To use it, set `model = "canary-1b-v2-q8_0.gguf"` and run `vocaline setup` again.

## The engine's patch

Under 2 s, CrispASR never decodes a phrase again: its final is decoded on that short slice
alone, often too little for Parakeet to settle on the language, so a short French phrase comes
out in English. `packaging/arch/crispasr-short-context.patch` adds `--stream-short-context-sec
N` to CrispASR's stream mode: such a phrase is decoded again with up to N s of the audio before
it (from the previous phrase), and only the words after the silence between them are kept.
Vocaline passes 10 s (`short_context_sec`). On 144 short synthetic French phrases: not French
17 → 4, right 114 → 131, about 0.05 s more before a short final and 70 MiB more VRAM. With the
option off, CrispASR's output is the same as without the patch. The patch is Vocaline's own,
not part of CrispASR; it is installed with CrispASR's licence.

## Building it yourself

`cargo build --release` gives a `vocaline` that looks `crispasr` up in `PATH`; set `command`
to use another one. The package itself is built by `makepkg` in `packaging/arch`, CrispASR
with it (long: CUDA for six GPU architectures).

To build only the patched CrispASR, from a clone of it at `v0.8.42` with its submodules:

```sh
git apply /path/to/vocaline/packaging/arch/crispasr-short-context.patch
cmake -B build -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=ON -DGGML_BACKEND_DL=ON \
  -DGGML_NATIVE=OFF -DGGML_CPU_ALL_VARIANTS=ON -DGGML_CUDA=ON \
  -DCMAKE_LIBRARY_OUTPUT_DIRECTORY="$PWD/build/bin" -DCMAKE_BUILD_WITH_INSTALL_RPATH=ON \
  -DCMAKE_INSTALL_RPATH='$ORIGIN'
cmake --build build --target crispasr-cli   # build/bin: crispasr and its libraries
```

`packaging/arch/PKGBUILD` has the exact flags. An upstream CrispASR (a release bundle:
`crispasr-linux-x86_64-cuda13.tar.gz`, `-vulkan`, or the CPU one) works too, without the
patch: it does not know the option and refuses to start with it, so set `short_context_sec = 0`.
