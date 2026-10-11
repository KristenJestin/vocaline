# AGENTS.md — Vocaline

Instructions for any coding agent working in this repository. `CLAUDE.md` is a symbolic link
to this file: there is one contract, not two that drift apart.

## Project overview

Vocaline is a local dictation tool for Linux (Wayland, Hyprland), written in Rust. While you
speak, the phrase being heard is shown as it forms (grey, still changing); at each pause the
phrase is frozen and pasted where you were typing. Nothing leaves the machine.

- **The engine** is CrispASR (C++/ggml, MIT) running NVIDIA Parakeet TDT 0.6B v3 (multilingual,
  very good French), on the GPU through CUDA where there is one. The speech is cut into
  utterances by a voice activity detector (Silero VAD); the open utterance is decoded again
  every few hundred milliseconds (a *partial*), then a last time after a short silence (the
  *final*).
- **A daemon** owns the microphone and the engine, and publishes a stream of events
  (listening, level, partial, final, silence, state). Every client reads that stream: the
  command line, a desktop shell that draws the partial phrase, a voice assistant that only
  listens.
- **Commands** drive it: `press` / `release` for a key, `start` / `stop` / `toggle`, `watch`
  for the event stream, `last` for the last text. Vocaline grabs no key itself: the desktop
  binds a key to these commands.
- **What makes it ours**: a vocabulary of one's own (names the model gets wrong), rules per
  application, statistics, and a configuration per machine (model, backend, cadence), so a
  machine without a GPU runs a lighter setup.

Vocaline is used alone. The maintainer's desktop configuration integrates it (package, key,
sounds, the shell that shows the partial phrase) from its own repository; nothing of that
desktop lives here.

## Commands

```
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
tools/commit-message.sh --range origin/dev..HEAD
git config core.hooksPath .githooks     # once per clone: the commit-msg and pre-commit hooks
```

## Principles

### 1. Think before coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them; don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If the code and the issue disagree, the issue wins; if the issue is wrong, say so instead of
  quietly deviating.

### 2. Simplicity first

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what the task asks. No abstractions for single-use code.
- No configurability that was not requested.
- No error handling for impossible scenarios. If 200 lines could be 50, rewrite.

### 3. Surgical changes

**Touch only what you must. Clean up only your own mess.**

- Don't "improve" adjacent code, comments or formatting. Don't refactor what isn't broken.
- Match existing style. If you notice unrelated dead code, mention it, don't delete it.
- Every changed line must trace directly to the task.

### 4. The failing test first

**Define success criteria. Loop until verified.**

- Write the test that fails first, see it fail, then make it pass.
- "Fix the bug" → a test that reproduces it, then the fix.
- Every scenario of the issue gets a test named after it. A scenario without a test is a
  defect, not a TODO.

## Code style

- Rust 2024, `cargo fmt`, `cargo clippy -- -D warnings` clean.
- Code, identifiers, comments, docs, issues and commits are in English.
- The engine, the microphone, the clipboard and the window manager are reached through traits
  (ports), so the logic around them is tested with fakes.
- A comment says why, not what.

## Testing

- **Tests never touch a real microphone, a real GPU, the real clipboard, the real window
  manager or the real configuration.** They use fakes and temporary folders.
- Audio fixtures are short and synthetic, or recorded on purpose for the repository; never a
  private recording.
- What only a real machine can show (latency, CPU, VRAM, quality of the French, pasting into a
  real window) is measured by hand and reported in the pull request, with the numbers.
- A test that fails only under load is rerun alone before the failure is called real.

## How the work is organised

| Who | What |
|---|---|
| The maintainer | decides what is built, tries each tranche on their machine, and merges what they have not delegated |
| The lead agent | turns the maintainer's decisions into issues, launches the developing agents, reviews their pull requests, keeps the tracker honest, and never hides a failure |
| Developing agents | implement one issue each: the failing test first, then the code, the verification and a pull request; they never merge |
| CI | `verify`, `commit-messages` and `shape` on every pull request |

### The tracker

- **Issues are the tickets, and a ticket stands alone.** It holds the what, the where, the "how
  to verify" and the base branch, with no local path. If it lacks one of them, ask rather than
  guess.
- The plan is cut into **tranches**, labels `tranche:1`, `tranche:2`…, beside `type:*` and
  `area:*`. A tranche ends on something the maintainer can use, and they accept it on their
  machine.
- **`dev` is the integration branch** and the default branch: a branch starts from
  `origin/dev` and its pull request targets it. `main` holds released versions.

## Git and pull requests (non-negotiable)

- **Never commit, merge, rebase, push or force-push on `main` or `dev` directly.** If you are
  on one of these branches, create a branch first (`feat/<n>-<topic>`, `fix/<n>-<topic>`). The
  `pre-commit` hook refuses it.
- Never rewrite published history. No `--no-verify`.
- One commit = one intent. No `wip` commits. Don't mix formatting and logic in one commit.
- Commits use the Git user configured on the machine.
- The subject is `<type>(<scope>): <subject>`: the **scope is required**, the subject is 72
  characters at most, starts lowercase, has no final period, and the type is one of feat, fix,
  refactor, test, docs, chore, build, ci, perf. `tools/commit-message.sh` is the judge (the
  `commit-msg` hook and CI run it).
- Scopes: `engine`, `audio`, `daemon`, `events`, `cli`, `output`, `vocabulary`, `rules`,
  `stats`, `config`, `repo`, `ci`.
- **Pull requests are merged by squash only.** The title is a plain Angular subject. The
  description is written in the first person as the maintainer and ends with `Closes #<n>`.

## Security and privacy

- Nothing private goes into the repository, an issue, a pull request or a commit: no personal
  vocabulary, no recording of a real person, no private path or host.
- Secrets are never printed, logged or committed.
- Nothing is published outside the pull request: no gist, no upload, no paste service.
- What a person dictates is theirs: Vocaline never sends it anywhere, and logs it only where the
  user asked (`last`, statistics), never in a debug log by default.
- **A permission denial is never worked around**: do what remains, say what was refused.

## Working on a machine

- The machine is the maintainer's own, in use while you work. The GPU is shared with their
  desktop and other programs: never leave a model loaded when you are done, and say how much
  VRAM a run took.
- No artificial load: no stress runs, no parallel benchmark loops. Heavy builds run at low
  priority (`nice -n 19`), one at a time.
- Worktrees, logs, models, recordings and every other scratch file live outside the clone.
- Never touch the maintainer's running dictation service or their configuration: run your own
  instance with a temporary configuration and socket.
- Before removing a worktree, check it holds no uncommitted change and no unpushed commit
  (`git log HEAD --not --remotes`).

## GitHub gotchas

- Prefer REST (`gh api repos/…`) to GraphQL, which gets rate-limited.
- A body holding backticks or apostrophes is written to a file and passed with `--body-file`.

## When done

Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`, plus the
manual measurements the issue asks for, and report the real output. If something fails, say so;
don't claim green.
