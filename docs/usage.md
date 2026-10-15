# Using Vocaline

The daemon, the key and the paste. Back to the [README](../README.md); the event stream and the socket are in [`events.md`](events.md).

## The daemon and its commands

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
back as it was, with every type it held (a screenshot stays a screenshot), once the application
has read the phrase and at least 400 ms after the paste: the application reads the clipboard on
its own time after the shortcut, and a busy one (an Electron app whose main thread is held up)
reads it a second later. One that has not read it after 2 s is taken as having dropped the
shortcut. The shortcut is typed once the compositor holds the phrase, and 15 ms after the last
read of it by a clipboard history, which reads each new selection at once. The text is offered
with the `x-kde-passwordManagerHint` type, so a clipboard history that honours it keeps no copy
of what was dictated.

While a modifier is held (SUPER of SUPER+A, Ctrl, Alt, Shift), the paste waits for its release:
pressed with it, the shortcut would be another one.

It needs Hyprland with its Lua API (it is asked which window has the focus and which keys are
down), `wtype`, and a compositor with the Wayland ext-data-control protocol. `vocaline start
--no-paste`, or `paste = false`, leaves the phrases in the event stream only.
