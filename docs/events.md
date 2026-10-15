# The event stream and the control socket

This is a contract: a desktop shell (which draws the phrase being spoken) and a voice assistant
(which only listens) read it. A change to it is described here and in the pull request that
makes it.

**Version 1.** The version is raised when a line changes meaning or loses a field. Adding a
field or a new event type does not raise it: a reader ignores what it does not know.

## Where

The daemon listens on a Unix socket, `$XDG_RUNTIME_DIR/vocaline.sock`. A client connects,
writes one command on one line, and reads the answer.

| Command | Answer |
|---|---|
| `press` | the key went down: `stop` when loading or listening, otherwise `start` and remember when; answers the state reached |
| `release` | the key went up: `stop` if it is still loading or listening and the key was held at least `hold_ms` (250 ms by default) since the `press` that started it; otherwise nothing; answers the state |
| `start` | starts listening; answers the state reached |
| `start --rule <name>` | `start`, with the rule of that name governing the dictation whatever the window; an unknown name is refused (`{"error":"no rule is named <name>"}`) and published as an `error` event |
| `press --rule <name>` | `press`, with the rule applied if it starts a dictation |
| `start --no-paste` | `start`, but the final phrases of this dictation are not pasted, only streamed |
| `stop` | stops listening, the open phrase is finalised; answers the state reached |
| `toggle` | `start` when idle, `stop` when loading or listening |
| `status` | the current state |
| `watch` | the event stream, until the client hangs up |
| `last` | the last final phrases of this session (at most 20, oldest first), kept in memory only |

`press` and `release` are for one key bound to both (hold to talk, or tap to start and tap to
stop). A `release` with no `press` before it, or after a `press` that stopped the dictation, does
nothing. A `press` while stopping is refused, like `start`. `start`, `stop` and `toggle` are for
callers that are not a key. The event stream does not change with the rules per application (see
[`configuration.md`](configuration.md#rules-per-application)): `partial` and `final` hold the phrases as heard, after the vocabulary,
whatever a rule does to the paste. The only trace of a rule in the stream is an `error` event
when a forced rule does not exist.

The answer to `press`, `release`, `start`, `stop`, `toggle` and `status` is one JSON line, then the daemon closes the
connection:

```json
{"state":"loading"}
{"error":"still stopping the last dictation, try again"}
```

The answer to `last` is one line too, with the phrases as they were shown (after the vocabulary):

```json
{"phrases":["Bonjour.","Ça va ?"]}
```

`vocaline watch` prints the stream to its standard output, so a program can also read it from
there.

## The events

One JSON object per line, UTF-8, `type` names the event.

```json
{"type":"hello","version":1}
{"type":"state","state":"loading"}
{"type":"state","state":"listening"}
{"type":"level","rms":0.031}
{"type":"silence"}
{"type":"partial","utterance":3,"text":"Bonjour, je voudrais"}
{"type":"partial","utterance":3,"text":"Bonjour, je voudrais tester la dictée"}
{"type":"final","utterance":3,"text":"Bonjour, je voudrais tester la dictée vocale."}
{"type":"state","state":"stopping"}
{"type":"state","state":"idle"}
```

| `type` | Fields | Meaning |
|---|---|---|
| `hello` | `version` | the first line of every `watch`; the version of this format |
| `state` | `state` | the daemon's state changed; the second line of every `watch` is the current state |
| `level` | `rms` | loudness of the last 100 ms of microphone audio, 0 to 1, ten per second while listening |
| `partial` | `utterance`, `text` | the phrase being spoken, as heard so far; the next partial of the same utterance replaces it |
| `final` | `utterance`, `text` | the phrase, frozen at the pause; it replaces the partials of its utterance and closes it |
| `silence` | | the engine heard no speech in its last step (one per step, every `partial_ms`) |
| `fallback` | `reason` | the GPU had no memory left for the engine, which died: the whole dictation is decoded again on the CPU once it stops; `reason` is the engine's line that said so |
| `error` | `message` | something went wrong (a model file is missing, the engine could not start or stopped, or stopped by itself, with its last lines of diagnostics, the statistics could not be written); the state follows |

The states:

| `state` | Meaning |
|---|---|
| `idle` | not listening; the model is not loaded |
| `loading` | the microphone is open and its audio kept; the engine is loading the model |
| `listening` | the engine is transcribing |
| `stopping` | the microphone is closed; the engine finishes the open phrase |

### What a reader can rely on

- `utterance` is a number that only grows for as long as the daemon runs, across dictations.
  An utterance ends with one `final`. The engine may still drop one it decides was not speech:
  a partial left open when the state comes back to `idle` is to be dropped.
- A `final` can arrive while `stopping`: stopping finalises the open phrase instead of losing
  it. `idle` comes after the last `final`.
- A partial can be empty or shorter than the previous one: it is the engine's latest reading,
  not a growing prefix. That reading is steadied: a word already shown changes only when two
  readings in a row agree on the change, so the phrase does not blink between two readings. The
  last word follows the engine at once. The `final` is not steadied.
- The text of `partial` and `final` is corrected by the user's vocabulary
  (`$XDG_CONFIG_HOME/vocaline/vocabulary.txt`, see [`configuration.md`](configuration.md#your-own-vocabulary)): the format does not change, only
  the words inside. The vocabulary is read when a dictation starts; a line of it that cannot be
  read is reported by an `error` event at that moment, and the state goes on.
- The filler words (`euh`, `uh`… the list is in the configuration, see [`configuration.md`](configuration.md#filler-words)) are dropped
  from `partial` and `final`, after the vocabulary, with the punctuation that hangs on them. A
  utterance made only of fillers still ends with one `final`, whose `text` is empty: a reader
  skips it. `last` and the statistics do not keep it.
- The paste joins the phrases of a dictation a pause has cut (the event stream does not: one
  `final` per phrase, as heard). A reader that wants the same text does the same: when a
  `final` ends with an ellipsis (`...` or `…`) or with no closing mark (`.`, `!`, `?`), drop the
  ellipsis and start the next one lowercase, unless its first word is `I`, in capitals, or has a
  capital inside; [`usage.md`](usage.md#pasting) gives the rule. Otherwise the two are joined by a space.
- A dictation the GPU had no memory for is not lost (since #38). The engine dies; the daemon
  publishes a `fallback` and stays `listening` until the dictation is stopped, the microphone
  still heard but nothing decoded. Once it stops, the whole dictation is decoded again on the
  CPU as one recording: one `final` under a new `utterance` number, with no `partial`, after the
  `stopping`, holds the text of all of it and replaces the phrases the dictation had before the
  `fallback` (the paste, `last` and the statistics hold only that one). A reader that keeps the
  phrases of a dictation drops them at the `fallback`. `idle` comes later than on the GPU: 1.6 s
  after the stop for 17 s of dictation, 10 s for a minute, on an 8-core desktop CPU.

  ```json
  {"type":"final","utterance":4,"text":"Bonjour"}
  {"type":"fallback","reason":"ggml_backend_cuda_buffer_type_alloc_buffer: allocating 232.73 MiB on device 0: cudaMalloc failed: out of memory"}
  {"type":"state","state":"stopping"}
  {"type":"final","utterance":5,"text":"Bonjour tout le monde, je voudrais tester la dictée."}
  {"type":"state","state":"idle"}
  ```
- A reader that cannot keep up is disconnected rather than slowing the daemon down.
