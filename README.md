# Vocaline

Local dictation for Linux: the phrase shows while you speak, and the dictation is pasted once, when it stops.
Parakeet through CrispASR, on the GPU when there is one. Nothing leaves the machine.

Work in progress: see the issues. Agents read `AGENTS.md`.

## Getting the engine and the models

Vocaline drives [CrispASR](https://github.com/CrispStrobe/CrispASR) (MIT) as a process. Nothing
of it is in this repository.

1. **CrispASR**: take the Linux bundle of a release (0.8.42 is the one tested), for example
   `crispasr-linux-x86_64-cuda13.tar.gz` for an NVIDIA GPU with CUDA 13,
   `crispasr-linux-x86_64-vulkan.tar.gz` for Vulkan, `crispasr-linux-x86_64.tar.gz` for the
   CPU. Unpack it anywhere: the `crispasr` binary finds its libraries next to itself.

   ```sh
   gh release download v0.8.42 -R CrispStrobe/CrispASR -p crispasr-linux-x86_64-cuda13.tar.gz
   tar xzf crispasr-linux-x86_64-cuda13.tar.gz
   ```

2. **Parakeet TDT 0.6B v3, quantised q8_0** (about 670 MB), published by CrispASR's author:

   ```sh
   mkdir -p ~/.local/share/vocaline/models
   curl -L -o ~/.local/share/vocaline/models/parakeet-tdt-0.6b-v3-q8_0.gguf \
     https://huggingface.co/cstr/parakeet-tdt-0.6b-v3-GGUF/resolve/main/parakeet-tdt-0.6b-v3-q8_0.gguf
   ```

   The same repository has `q5_0` and `q4_k` for a smaller machine. From the full
   `parakeet-tdt-0.6b-v3.gguf`, `crispasr-quantize` makes any of them.

   Parakeet guesses the language by itself and ignores `language`: a short phrase can come out
   in English (`allô` → `Hello?`). **Canary 1B v2** obeys `language`, at a cost: on the same
   GPU it takes about 3.5 times the CPU while you speak, 0.4 GB more VRAM and 0.15 s more
   before each final. To use it, download it the same way and set `model`:

   ```sh
   curl -L -o ~/.local/share/vocaline/models/canary-1b-v2-q8_0.gguf \
     https://huggingface.co/cstr/canary-1b-v2-GGUF/resolve/main/canary-1b-v2-q8_0.gguf
   ```

3. **Silero VAD**: nothing to do. With `vad_model = "auto"` (the default), CrispASR downloads
   `ggml-silero-v6.2.0.bin` from `ggml-org/whisper-vad` on Hugging Face once and keeps it in
   `~/.cache/crispasr`. Set `vad_model` to a path to use a file of your own.

## Configuration

`$XDG_CONFIG_HOME/vocaline/config.toml` (usually `~/.config/vocaline/config.toml`); every key is
optional, these are the defaults:

```toml
[engine]
command = "crispasr"                          # the binary: a path, or a name in PATH
model = "parakeet-tdt-0.6b-v3-q8_0.gguf"      # or canary-1b-v2-q8_0.gguf; relative: read from ~/.local/share/vocaline/models
vad_model = "auto"
backend = "cuda"                              # cuda, vulkan or cpu
language = "fr"
partial_ms = 400                              # the open phrase is decoded again this often
final_silence_ms = 700                        # this much silence ends a phrase

[key]
hold_ms = 250                                 # a key held this long stops on release (see below)

[output]
paste = true                                  # false: the phrases are only in the event stream
```

## Using it

```sh
vocaline daemon &      # owns the microphone (PipeWire, through pw-record) and the engine
vocaline watch         # the event stream, one JSON object per line (docs/events.md)
vocaline start         # or stop, toggle, status
```

The model is loaded when listening starts and unloaded when it stops.
`vocaline daemon --audio-file speech.wav` plays a 16 kHz mono recording instead of the
microphone at each `start`: to try a machine without speaking.

## One key for hold and tap

Vocaline grabs no key: bind `vocaline press` to the key going down and `vocaline release` to the
same key going up. On Hyprland:

```
bind = SUPER, A, exec, vocaline press
bindr = SUPER, A, exec, vocaline release
```

Hold the key while you speak and release it to stop. Or tap it (shorter than `hold_ms`): it keeps
listening, and the next press stops it. Either way, the phrase being spoken is finalised first.
`start`, `stop` and `toggle` stay for a button or another program.

## Pasting

Nothing is written while you speak: the phrases are cut at each pause and decoded right away,
but only the event stream has them (a desktop shell can show them as a live preview). When the
dictation stops (key released, second tap, `stop`), its phrases are joined by a space, in
order, and pasted once, as soon as the last open phrase is decoded. A dictation with no phrase
pastes nothing.

The text is pasted in the window that had the focus when the dictation started, even if the
focus moved since: the focus is given back to it first (if it was closed, the text goes where
the focus is).

The phrase goes through the clipboard and a paste shortcut typed by `wtype`: Ctrl+Shift+V in a
terminal (foot, Alacritty, kitty, Ghostty, WezTerm…), Ctrl+V elsewhere. The clipboard is put
back as it was, with every type it held (a screenshot stays a screenshot), 400 ms after the
paste: the application reads the clipboard on its own time after the shortcut. The text is
offered with the `x-kde-passwordManagerHint` type, so a clipboard history that honours it keeps
no copy of what was dictated.

While a modifier is held (SUPER of SUPER+A, Ctrl, Alt, Shift), the paste waits for its release:
pressed with it, the shortcut would be another one.

It needs Hyprland with its Lua API (it is asked which window has the focus and which keys are
down), `wtype`, and a compositor with the Wayland data-control protocol. `vocaline start
--no-paste`, or `paste = false`, leaves the phrases in the event stream only.
