# Vocaline

Local dictation for Linux: the phrase shows while you speak, and the dictation is pasted once, when it stops.
Parakeet through CrispASR, on the GPU when there is one. Nothing leaves the machine.

Work in progress: see the issues. Agents read `AGENTS.md`.

## Installing it (Arch Linux)

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
`/usr/lib/vocaline/crispasr`: the Linux CUDA 13 bundle of [CrispASR](https://github.com/CrispStrobe/CrispASR)
0.8.42 (MIT), which also holds the CPU backends. For the `cuda` backend, install `cuda` too.
The `vulkan` backend is not in it: point `command` at CrispASR's Vulkan bundle (below).
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

The unit is bound to `graphical-session.target`, which Hyprland started through `uwsm`
reaches. The daemon pastes through Wayland and asks Hyprland which window has the focus: the
user manager must have `WAYLAND_DISPLAY` and `HYPRLAND_INSTANCE_SIGNATURE` in its environment
(`uwsm` does it; otherwise `systemctl --user import-environment WAYLAND_DISPLAY
HYPRLAND_INSTANCE_SIGNATURE` at login).

## Choosing a model

Parakeet guesses the language by itself and ignores `language`: a short phrase can come out
in English (`allô` → `Hello?`). **Canary 1B v2** obeys `language`, at a cost: on the same GPU
it takes about 3.5 times the CPU while you speak, 0.4 GB more VRAM and 0.15 s more before each
final. To use it, set `model = "canary-1b-v2-q8_0.gguf"` and run `vocaline setup` again.

## Building it yourself

`cargo build --release` gives a `vocaline` that looks `crispasr` up in `PATH`; set `command`
to use another one. Take the Linux bundle of a CrispASR release (0.8.42 is the one tested):
`crispasr-linux-x86_64-cuda13.tar.gz` for an NVIDIA GPU with CUDA 13,
`crispasr-linux-x86_64-vulkan.tar.gz` for Vulkan, `crispasr-linux-x86_64.tar.gz` for the CPU,
and unpack it anywhere: the `crispasr` binary finds its libraries next to itself. The package
itself is built by `makepkg` in `packaging/arch`.

## Configuration

`$XDG_CONFIG_HOME/vocaline/config.toml` (usually `~/.config/vocaline/config.toml`); every key is
optional, these are the defaults:

```toml
[engine]
command = "/usr/lib/vocaline/crispasr/crispasr" # the package's engine ("crispasr", from PATH, built by hand)
model = "parakeet-tdt-0.6b-v3-q8_0.gguf"      # or canary-1b-v2-q8_0.gguf; relative: read from ~/.local/share/vocaline/models
vad_model = "ggml-silero-v6.2.0.bin"          # relative: read from the models folder; "auto": CrispASR fetches it
backend = "cuda"                              # cuda, vulkan or cpu
language = "fr"
partial_ms = 400                              # the open phrase is decoded again this often
final_silence_ms = 700                        # this much silence ends a phrase

[key]
hold_ms = 250                                 # a key held this long stops on release (see below)

[output]
paste = true                                  # false: the phrases are only in the event stream

[text]
filler_words = ["euh", "heu", "euhm", "hum", "hmm", "mmh", "uh", "um", "mm"]  # dropped from every phrase; []: off
```

Rules per application are `[[rule]]` tables, none by default: see "Rules per application".

## Using it

```sh
vocaline daemon &      # owns the microphone (PipeWire, through pw-record) and the engine; or the unit
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

Showing the phrase while you speak is the desktop's job: any program (a bar, a notification, a
voice assistant) can read `vocaline watch`.

## Pasting

Nothing is written while you speak: the phrases are cut at each pause and decoded right away,
but only the event stream has them (a desktop shell can show them as a live preview). When the
dictation stops (key released, second tap, `stop`), its phrases are joined, in order, and pasted
once, as soon as the last open phrase is decoded. A dictation with no phrase pastes nothing.

The pause that cuts a phrase does not always end a sentence: the model punctuates each phrase
alone. When a phrase ends with an ellipsis (`...` or `…`) or with no closing mark (`.`, `!`,
`?`), the next one goes on from it: in the pasted text the ellipsis goes and the next phrase
starts lowercase (`Je voulais dire... Que ça marche.` is pasted as `Je voulais dire que ça
marche.`). A phrase that ended (`.`, `!`, `?`) is followed by a space and keeps its capital. A
capital that is not the start of a sentence stays: `I`, a word in capitals (`NASA`), a word with
a capital inside, a word your vocabulary writes with one. The ellipsis that ends the whole text
stays. Only the pasted text is joined: `vocaline last` and the event stream keep one phrase each,
as shown. With a rule's `drop_period`, the join comes first, then the period that ends the whole
text goes.

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

## Rules per application

What you want after a dictation depends on where you dictate: a chat sends it, a terminal runs it,
an assistant only listens. A rule says so for a window class (`hyprctl activewindow` shows it):

```toml
[[rule]]
name = "terminal"
class = "foot"           # case ignored; * stands for any run of characters
enter = true             # press Enter once, after the paste
drop_period = true       # no period at the end of the pasted text

[[rule]]
name = "chat"
class = "org.example.*"
enter = true
vocabulary = ["foo bar -> Foobar"]   # same lines as vocabulary.txt, added to it

[[rule]]
name = "assistant"
paste = false            # nothing is pasted, the phrases stay in the event stream
```

- **The first rule whose `class` matches wins**; a window that matches none gets the defaults
  (paste, no Enter, the text as heard). The class is the one of the window that had the focus
  when the dictation **started**, the same window the text is pasted in, not where the focus is
  when it stops.
- **`paste`** (default `true`), **`enter`** (default `false`) and **`drop_period`** (default
  `false`) are all a rule can change about the paste. `enter` presses Return once, after the one
  paste (and after the application has had time to read it, so a terminal runs the whole line);
  it presses nothing when nothing was pasted. `drop_period` removes the period that ends the
  whole text, not the ones inside it, and leaves `?`, `!` and an ellipsis alone. The `final`
  events and `vocaline last` keep the phrases as heard: only the paste changes.
- **`vocabulary`** adds lines (the format of the next section) to the common vocabulary, for the
  dictations this rule governs; on the same heard words, the rule wins. It corrects the
  partials, the finals and the paste, like the common one.
- A rule needs a one-word `name`, used once. Without a `class` it never matches a window: it is
  only reached by name. A mistake in a rule (unknown key, bad vocabulary line, repeated name)
  stops the daemon at startup with the rule named.
- With `paste = false` under `[output]`, or `start --no-paste`, nothing is pasted and the
  window is not asked: no rule is chosen by window, and `--no-paste` takes no `--rule`. Under
  `[output]`, a forced rule still adds its vocabulary.

### Forcing a rule

`vocaline start --rule terminal` makes the rule of that name govern the dictation, whatever the
window, for a key bound to one use. So does `vocaline press --rule terminal`, because the key of
the previous section is a `press` / `release` pair and the rule is only needed when the key
starts the dictation (`release` takes none):

```
bind = SUPER, T, exec, vocaline press --rule terminal
bindr = SUPER, T, exec, vocaline release
```

A name that is not a rule is refused before the microphone opens, and said in an `error` event.
It governs that one dictation only. `toggle` takes no rule: use `start --rule` and `stop`.

## Your own vocabulary

Parakeet is very good in French but still writes some names its own way. After you see a
mistake, one command fixes it for good:

```sh
vocaline teach "foo bar" "Foobar"
```

It adds a line to `$XDG_CONFIG_HOME/vocaline/vocabulary.txt` (usually
`~/.config/vocaline/vocabulary.txt`), a plain file you can edit by hand:

```
# what the model writes -> what I want
foo bar -> Foobar
élodie -> Elodie
```

One entry per line; empty lines and lines starting with `#` are ignored; when two lines hear the
same words the later one wins. Matching ignores case and accents, and respects word boundaries:
`bar` changes `bar` and `sous-bar` but not `barbare`. An entry of several words matches them
across any run of spaces. A line that cannot be read is named in an `error` event and left out;
the others still apply.

The correction happens where the engine's text enters the daemon, so the partial phrase, the
final phrase (both in the event stream, see `docs/events.md`) and the pasted text hold the same
words. The file is read again when a dictation starts: an entry applies from the next dictation,
and `teach` works with no daemon running.

`vocaline last` prints the last 20 final phrases of the session, as they were shown, one per
line. They are kept in the daemon's memory only: never written to disk or to a log, and gone when
the daemon stops.

## Filler words

The model writes everything that is said, hesitations included: `Mais surtout sur la partie
euh... Vocaline et tout.` Vocaline drops the words of `filler_words` (see "Configuration")
and the punctuation that hangs on them, so that phrase becomes `Mais surtout sur la partie
Vocaline et tout.` No model is involved and it costs nothing.

- Whole words only, case and accents ignored, like the vocabulary: `euh` goes, `heureux` and
  `humain` stay. The words that carry a meaning (`bah`, `ben`, `bon`, `oui`, `yeah`) are not in
  the default list; add the ones you want gone. An empty list (`filler_words = []`) turns the
  step off.
- A filler that started a phrase leaves the next word with a capital (`Euh, donc on y va.`
  becomes `Donc on y va.`). A `.`, `!` or `?` hanging on a last filler goes back to the word
  before it (`on y va, euh.` becomes `on y va.`); an ellipsis goes with it.
- A phrase made only of fillers disappears: it is not pasted, not in `vocaline last`, and
  it counts for no word in the statistics. Its `final` event still closes the utterance, with an
  empty `text`.
- It happens right after the vocabulary, where the engine's text comes in, so the partial
  phrase, the final phrase and the pasted text hold the same words. `mm` is in the list because
  the model writes it for a hum: if you dictate millimetres, take it out.

## Statistics

```sh
vocaline stats           # today, the last 7 days, all time
vocaline stats --json    # the same, as one JSON object, for a program
```

For each period: the dictations, the words (without the filler words), the minutes heard, the time from the stop to the
paste (median and maximum), and the same by application (the class of the window the dictation
started in) and, for the last 7 days, by day. A dictation that pastes nothing (`start --no-paste`,
`paste = false`) does not ask for the window, so it is counted under `unknown`, like one whose
window the desktop could not name. A dictation in which nothing was heard is not counted.

The daemon appends one line per dictation to `$XDG_DATA_HOME/vocaline/stats.jsonl` (usually
`~/.local/share/vocaline/stats.jsonl`) when the dictation ends; `stats` reads that file itself, so
it works with no daemon running. Removing the package leaves it, like the configuration; delete
it to start over.

**No dictated text is stored**: a line holds the day, the class of the window, a word count, the
audio duration in milliseconds and the time to paste in milliseconds. Not a word of the text,
not even a window title. If the file cannot be written, an `error` event says so and the
dictation is pasted all the same.

The time to paste runs from the stop (second tap, release, `stop`) to the moment the text is
handed to the output, which then pastes it in a few tens of milliseconds more. The minutes are
the time the microphone was heard, silences included.
