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
callers that are not a key. The event stream does not change with the rules per application (the
README describes them): `partial` and `final` hold the phrases as heard, after the vocabulary,
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
| `error` | `message` | something went wrong (the engine could not start or stopped, the statistics could not be written); the state follows |

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
  (`$XDG_CONFIG_HOME/vocaline/vocabulary.txt`, see the README): the format does not change, only
  the words inside. The vocabulary is read when a dictation starts; a line of it that cannot be
  read is reported by an `error` event at that moment, and the state goes on.
- A reader that cannot keep up is disconnected rather than slowing the daemon down.
