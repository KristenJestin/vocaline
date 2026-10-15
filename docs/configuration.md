# Configuring Vocaline

The configuration file, the rules per application, your vocabulary, the filler words and the statistics. Back to the [README](../README.md).

## The configuration file

`$XDG_CONFIG_HOME/vocaline/config.toml` (usually `~/.config/vocaline/config.toml`); every key is
optional, these are the defaults:

```toml
[engine]
command = "/usr/lib/vocaline/crispasr/crispasr" # the package's engine ("crispasr", from PATH, built by hand)
model = "parakeet-tdt-0.6b-v3-q8_0.gguf"      # or canary-1b-v2-q8_0.gguf; relative: read from ~/.local/share/vocaline/models
vad_model = "ggml-silero-v6.2.0.bin"          # relative: read from the models folder; "auto": CrispASR fetches it
backend = "cuda"                              # cuda, vulkan or cpu
language = "fr"
partial_ms = 400                              # the engine's step: how often it listens, so how soon the end of a phrase is seen
# partial_decode_ms = 1500                    # not set: the open phrase is decoded at every step; set, this often (0: no grey text, finals only)
final_silence_ms = 700                        # this much silence ends a phrase
short_context_sec = 10                        # a phrase under 2 s is decoded again with this much audio before it; 0: off (an upstream CrispASR)

[key]
hold_ms = 250                                 # a key held this long stops on release (see below)

[output]
paste = true                                  # false: the phrases are only in the event stream

[text]
filler_words = ["euh", "heu", "euhm", "hum", "hmm", "mmh", "uh", "um", "mm"]  # dropped from every phrase; []: off
```

Rules per application are `[[rule]]` tables, none by default: see "Rules per application".

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
[One key for hold and tap](usage.md#one-key-for-hold-and-tap) is a `press` / `release` pair and the rule is only needed when the key
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
euh... Vocaline et tout.` Vocaline drops the words of `filler_words` (see "The configuration file")
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
  the model writes it for a hum; right after a number (`5 mm`, `2,5 mm`) a filler word is a
  unit, not a hesitation, and is kept.

## Statistics

```sh
vocaline stats           # today, the last 7 days, all time
vocaline stats --json    # the same, as one JSON object, for a program
```

For each period: the dictations, the words (without the filler words), the minutes heard, the time from the stop to the
paste (median and maximum), and the same by application (the class of the window the dictation
started in) and, for the last 7 days, by day, and how many dictations the GPU had no memory
for, which were decoded on the CPU instead (the line shows only when there is one). A dictation that pastes nothing (`start --no-paste`,
`paste = false`) does not ask for the window, so it is counted under `unknown`, like one whose
window the desktop could not name. A dictation in which nothing was heard is not counted.

The daemon appends one line per dictation to `$XDG_DATA_HOME/vocaline/stats.jsonl` (usually
`~/.local/share/vocaline/stats.jsonl`) when the dictation ends; `stats` reads that file itself, so
it works with no daemon running. Removing the package leaves it, like the configuration; delete
it to start over.

**No dictated text is stored**: a line holds the day, the class of the window, a word count, the
audio duration in milliseconds, the time to paste in milliseconds and, for a dictation decoded
on the CPU because the GPU was full, `"cpu_fallback":true`. Not a word of the text,
not even a window title. If the file cannot be written, an `error` event says so and the
dictation is pasted all the same.

The time to paste runs from the stop (second tap, release, `stop`) to the moment the text is
handed to the output, which then pastes it in a few tens of milliseconds more. The minutes are
the time the microphone was heard, silences included.
