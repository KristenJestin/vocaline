#!/usr/bin/env python3
"""The engine bench: what a setup costs and whether it keeps short phrases in French.

Every number comes from a real daemon of this repository, run as its own instance (its own
configuration, socket and data folder under WORK), fed synthetic French recordings at the pace
of real time with `daemon --audio-file`, one dictation at a time.

    tools/bench.py prepare WORK
        Downloads the Piper voices and the models into WORK, then synthesises the recordings.
    tools/bench.py run WORK LABEL VOCALINE --crispasr PATH [--engine 'key = value']...
                   [--runs N] [--only short,long,clips]
        Measures one setup; one JSON line per dictation goes to WORK/results.jsonl.
    tools/bench.py table WORK
        One markdown row per label.

What is measured, per dictation:
- CPU: the daemon's reaped-children counters (/proc/<pid>/stat fields 16-17) read before
  `start` and once the daemon is idle again, so every engine process it ran is counted, divided
  by the time spent listening. The engine's own counter, last seen while it ran, is kept beside
  it as a check.
- VRAM: the memory nvidia-smi gives the daemon's children, sampled every 200 ms; the peak.
- Latency: from the start of speech to the first partial of the phrase, and from its end to its
  final. The first phrase of a dictation also waits for the model to load: it is left out.
- Language: 24 short phrases in 6 sets (two voices, clean, faster, through a cheap microphone):
  144 clips. A final is not French when it holds a word of FOREIGN, a list made from what the
  engines were seen writing. Every final that is not the phrase is kept in the results.

Needs python3, ffmpeg, nvidia-smi, uvx (for Piper) and curl. Nothing of the machine is read
besides WORK and the binaries given.
"""

import json
import os
import re
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import threading
import time
import unicodedata
import wave
from array import array
from pathlib import Path

RATE = 16000
PIPER = "piper-tts==1.8.0"
VOICES = {
    "siwis": "fr/fr_FR/siwis/medium/fr_FR-siwis-medium",
    "tom": "fr/fr_FR/tom/medium/fr_FR-tom-medium",
}
VOICE_URL = "https://huggingface.co/rhasspy/piper-voices/resolve/main/{}.onnx"
MODELS = {
    "parakeet-tdt-0.6b-v3-q8_0.gguf": "https://huggingface.co/cstr/parakeet-tdt-0.6b-v3-GGUF/resolve/main/parakeet-tdt-0.6b-v3-q8_0.gguf",
    "canary-1b-v2-q8_0.gguf": "https://huggingface.co/cstr/canary-1b-v2-GGUF/resolve/main/canary-1b-v2-q8_0.gguf",
}

SHORT_PHRASES = [
    "Il faudrait envoyer le compte rendu avant la réunion de jeudi.",
    "Je relis la dernière partie et je te dis ce que j'en pense.",
    "Pense aussi à réserver la salle pour la semaine prochaine.",
]
LONG_SENTENCES = [
    "Il s'accompagne d'un autre projet qui permet de stocker les notes de la semaine dans un seul endroit",
    "on y retrouve les idées, les rendez-vous et les choses à ne pas oublier",
    "le but est de pouvoir chercher rapidement une information sans ouvrir dix fichiers différents",
    "chaque note est rattachée à une personne de manière simple, avec son nom et la date",
    "quand on parle d'un sujet, le programme propose les notes qui lui ressemblent",
    "il faut aussi que tout reste sur la machine, sans rien envoyer sur un serveur",
    "pour l'instant la recherche marche bien sur les mots exacts, un peu moins sur les synonymes",
    "la prochaine étape consiste à relier les notes entre elles quand elles parlent du même projet",
    "ensuite il faudra une petite interface pour les relire le soir avant de fermer l'ordinateur",
    "on pourra aussi exporter une semaine entière dans un document unique et bien rangé",
    "enfin, si tout se passe bien, on ajoutera la dictée pour écrire une note sans clavier",
    "cela suffit pour commencer, le reste viendra plus tard selon les besoins de chacun",
]
# Breaths between the sentences of the long recording: all shorter than the 700 ms that end a
# phrase, so it is one utterance of about a minute.
LONG_BREATHS = [0.35, 0.5, 0.25, 0.45, 0.6, 0.3, 0.55, 0.4, 0.5, 0.35, 0.45]
CLIP_PHRASES = [
    "Allô ?", "Allô, allô.", "Allô allô allô.", "Je m'arrête.", "Oui.", "D'accord.", "Non.",
    "Merci.", "Bonjour.", "Salut.", "C'est bon.", "Très bien.", "Pas encore.", "On y va.",
    "Je reviens.", "Bien sûr.", "Ça marche.", "Au revoir.", "Voilà.", "Attends.", "Entendu.",
    "Parfait.", "Je dis allô.", "Stop.",
]
# voice, Piper length scale (below 1 speaks faster), through a cheap microphone
CLIP_SETS = {
    "siwis": ("siwis", 1.0, False),
    "siwis-fast": ("siwis", 0.75, False),
    "siwis-mic": ("siwis", 1.0, True),
    "tom": ("tom", 1.0, False),
    "tom-fast": ("tom", 0.75, False),
    "tom-mic": ("tom", 1.0, True),
}
FOREIGN = re.compile(
    r"\b(hello|yeah|no|up|what|uh|oh|mm|mm-hmm|well|i|i'm|i've|i'll|don't|know|step|stupid|stuck"
    r"|said|you|we|tap|top|entonces|stip|the|that|but|should|how|really|about|okay|yes|thank"
    r"|thanks|bye|good|right|and|is|it|so)\b|[а-яё]"
)


def main(argv):
    if len(argv) >= 2 and argv[0] == "prepare":
        prepare(Path(argv[1]))
    elif len(argv) >= 4 and argv[0] == "run":
        run(Path(argv[1]).resolve(), argv[2], Path(argv[3]).resolve(), argv[4:])
    elif len(argv) >= 2 and argv[0] == "table":
        table(Path(argv[1]))
    else:
        sys.exit(__doc__)


# ---------------------------------------------------------------------------------------------
# prepare


def prepare(work):
    voices = work / "voices"
    models = work / "data" / "vocaline" / "models"
    audio = work / "audio"
    for folder in (voices, models, audio):
        folder.mkdir(parents=True, exist_ok=True)
    for name, path in VOICES.items():
        fetch(VOICE_URL.format(path), voices / f"{name}.onnx")
        fetch(VOICE_URL.format(path) + ".json", voices / f"{name}.onnx.json")
    for name, url in MODELS.items():
        fetch(url, models / name)

    make_recording(audio / "short", voices / "siwis.onnx", SHORT_PHRASES, [2.0] * 2, 1.0, False)
    make_recording(audio / "long", voices / "siwis.onnx", LONG_SENTENCES, LONG_BREATHS, 1.0, False)
    for name, (voice, scale, mic) in CLIP_SETS.items():
        gaps = [2.5] * (len(CLIP_PHRASES) - 1)
        make_recording(audio / f"clips-{name}", voices / f"{voice}.onnx", CLIP_PHRASES, gaps, scale, mic)


def fetch(url, path):
    if path.exists():
        return
    print(f"fetching {path.name}", file=sys.stderr)
    part = path.with_suffix(path.suffix + ".part")
    subprocess.run(["curl", "-fL", "--silent", "--show-error", "-o", part, url], check=True)
    part.rename(path)


def make_recording(base, voice, phrases, gaps, scale, mic):
    """base.wav, and base.json: where each phrase starts and ends, and what it says."""
    if base.with_suffix(".json").exists():
        return
    print(f"synthesising {base.name}", file=sys.stderr)
    samples = array("h", [0] * (RATE // 2))
    segments = []
    spoken = synthesise(voice, phrases, scale, base.with_name(base.name + ".parts"))
    for i, text in enumerate(phrases):
        voiced = trim(spoken[i])
        segments.append({"start": len(samples) / RATE, "end": (len(samples) + len(voiced)) / RATE, "text": text})
        samples.extend(voiced)
        pause = gaps[i] if i < len(gaps) else 2.0
        samples.extend([0] * int(pause * RATE))
    plain = base.with_name(base.name + ".plain.wav")
    write_wav(plain, samples)
    if mic:
        # A cheap microphone: a narrow band and a pink noise floor.
        subprocess.run(
            ["ffmpeg", "-loglevel", "error", "-y", "-i", plain,
             "-f", "lavfi", "-i", f"anoisesrc=color=pink:amplitude=0.015:sample_rate={RATE}:seed=17",
             "-filter_complex", "[0:a]highpass=f=150,lowpass=f=3800[v];[v][1:a]amix=inputs=2:duration=first:normalize=0",
             "-ar", str(RATE), "-ac", "1", "-c:a", "pcm_s16le", base.with_suffix(".wav")],
            check=True,
        )
        plain.unlink()
    else:
        plain.rename(base.with_suffix(".wav"))
    base.with_suffix(".json").write_text(json.dumps(segments, ensure_ascii=False, indent=1))


# Run inside Piper's environment: one voice loaded once, one WAV per line of the input.
SYNTHESISE = """
import sys, wave
from piper import PiperVoice, SynthesisConfig
voice = PiperVoice.load(sys.argv[1])
config = SynthesisConfig(length_scale=float(sys.argv[2]))
for i, text in enumerate(sys.stdin.read().splitlines()):
    with wave.open(f"{sys.argv[3]}/{i}.wav", "wb") as w:
        voice.synthesize_wav(text, w, syn_config=config)
"""


def synthesise(voice, phrases, scale, folder):
    folder.mkdir(exist_ok=True)
    subprocess.run(["uvx", "--from", PIPER, "python", "-c", SYNTHESISE, voice, str(scale), folder],
                   input="\n".join(phrases).encode(), check=True)
    out = []
    for i in range(len(phrases)):
        pcm = subprocess.run(
            ["ffmpeg", "-loglevel", "error", "-i", folder / f"{i}.wav", "-ar", str(RATE), "-ac", "1", "-f", "s16le", "-"],
            capture_output=True, check=True,
        ).stdout
        out.append(array("h", pcm))
    shutil.rmtree(folder)
    return out


def trim(samples):
    """Piper's own silence around the phrase, so the bench knows where speech starts and ends."""
    loud = [i for i, s in enumerate(samples) if abs(s) > 300]
    return samples[loud[0] : loud[-1] + 1] if loud else samples


def write_wav(path, samples):
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(samples.tobytes())


# ---------------------------------------------------------------------------------------------
# run


def run(work, label, vocaline, args):
    crispasr, engine, runs, only = None, [], 2, ["short", "long", "clips"]
    while args:
        flag, value, args = args[0], args[1], args[2:]
        if flag == "--crispasr":
            crispasr = Path(value).resolve()
        elif flag == "--engine":
            engine.append(value)
        elif flag == "--runs":
            runs = int(value)
        elif flag == "--only":
            only = value.split(",")
        else:
            sys.exit(f"unknown option {flag}")
    if not crispasr:
        sys.exit("--crispasr PATH is required")

    recordings = []
    for name in only:
        if name == "clips":
            recordings += [(f"clips-{s}", 1) for s in CLIP_SETS]
        else:
            recordings.append((name, runs))

    instance = work / "instance"
    config = instance / "config" / "vocaline"
    runtime = instance / "runtime"
    config.mkdir(parents=True, exist_ok=True)
    runtime.mkdir(parents=True, exist_ok=True)
    runtime.chmod(0o700)
    (config / "config.toml").write_text(
        "[engine]\n"
        f'command = "{crispasr}"\n'
        + "".join(f"{line}\n" for line in engine)
        + "\n[output]\npaste = false\n"
    )
    env = dict(os.environ, XDG_CONFIG_HOME=str(instance / "config"), XDG_RUNTIME_DIR=str(runtime),
               XDG_DATA_HOME=str(work / "data"))
    env.pop("WAYLAND_DISPLAY", None)
    logs = work / "logs"
    logs.mkdir(exist_ok=True)

    with open(work / "results.jsonl", "a") as results:
        for name, count in recordings:
            for n in range(1, count + 1):
                wait_for_a_free_engine()
                result = dictation(vocaline, env, runtime / "vocaline.sock", work / "audio" / name, logs / f"{label}-{name}-{n}")
                result.update(label=label, recording=name, run=n, engine=engine)
                results.write(json.dumps(result, ensure_ascii=False) + "\n")
                results.flush()
                print(summary_line(result), file=sys.stderr)


def wait_for_a_free_engine():
    """One engine at a time on the machine: another one would share the GPU and skew the numbers."""
    told = False
    while subprocess.run(["pgrep", "-x", "crispasr"], capture_output=True).returncode == 0:
        if not told:
            print("another engine runs; waiting for it to end", file=sys.stderr)
            told = True
        time.sleep(2)


def dictation(vocaline, env, sock, recording, log):
    segments = json.loads(recording.with_suffix(".json").read_text())
    daemon = subprocess.Popen([vocaline, "daemon", "--audio-file", recording.with_suffix(".wav")], env=env,
                              stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                              stderr=open(log.with_suffix(".daemon.err"), "w"))
    try:
        until(lambda: connects(sock), 10)
        watch = subprocess.Popen([vocaline, "watch"], env=env, stdout=subprocess.PIPE, text=True)
        events = []
        reader = threading.Thread(target=read_events, args=(watch.stdout, events), daemon=True)
        reader.start()
        until(lambda: any(e.get("type") == "state" for _, e in events), 10)
        sampler = Sampler(daemon.pid)
        cpu_before = children_cpu(daemon.pid)
        subprocess.run([vocaline, "start", "--no-paste"], env=env, check=True, capture_output=True)
        duration = segments[-1]["end"] + 3
        until(lambda: idle_after_listening(events), duration + 120)
        time.sleep(0.3)
        cpu = children_cpu(daemon.pid) - cpu_before
        sampler.stop()
        watch.terminate()
        reader.join(timeout=2)
    finally:
        daemon.send_signal(signal.SIGTERM)
        daemon.wait(timeout=10)
    with open(log.with_suffix(".events"), "w") as f:
        for t, e in events:
            if e.get("type") != "level":
                f.write(f"{t:.3f}\t{json.dumps(e, ensure_ascii=False)}\n")
    return measure(events, segments, cpu, sampler)


def connects(path):
    try:
        with socket.socket(socket.AF_UNIX) as s:
            s.connect(str(path))
        return True
    except OSError:
        return False


def until(condition, timeout):
    end = time.monotonic() + timeout
    while not condition():
        if time.monotonic() > end:
            raise TimeoutError("the daemon did not get there in time")
        time.sleep(0.05)


def read_events(stream, events):
    for line in stream:
        try:
            events.append((time.monotonic(), json.loads(line)))
        except ValueError:
            pass


def idle_after_listening(events):
    states = [e["state"] for _, e in events if e.get("type") == "state"]
    return "listening" in states and states[-1] == "idle" and states.index("listening") < len(states) - 1


def children_cpu(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    # fields[0] is field 3 of stat(5): cutime and cstime are fields 16 and 17.
    return (int(fields[13]) + int(fields[14])) / os.sysconf("SC_CLK_TCK")


class Sampler:
    """VRAM of the daemon's children and their own CPU counters, every 200 ms."""

    def __init__(self, pid):
        self.pid, self.vram_peak, self.own_cpu = pid, 0, {}
        self.running = True
        self.thread = threading.Thread(target=self.loop, daemon=True)
        self.thread.start()

    def loop(self):
        tick = os.sysconf("SC_CLK_TCK")
        while self.running:
            children = child_pids(self.pid)
            for child in children:
                try:
                    f = Path(f"/proc/{child}/stat").read_text().rsplit(")", 1)[1].split()
                    self.own_cpu[child] = (int(f[11]) + int(f[12])) / tick
                except (OSError, IndexError):
                    pass
            if children:
                used = subprocess.run(
                    ["nvidia-smi", "--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"],
                    capture_output=True, text=True,
                ).stdout
                mib = sum(int(m) for p, m in (l.split(", ") for l in used.splitlines() if ", " in l) if int(p) in children)
                self.vram_peak = max(self.vram_peak, mib)
            time.sleep(0.2)

    def stop(self):
        self.running = False
        self.thread.join()


def child_pids(pid):
    found = set()
    for task in Path(f"/proc/{pid}/task").glob("*"):
        try:
            found.update(int(c) for c in (task / "children").read_text().split())
        except OSError:
            pass
    return found


def measure(events, segments, cpu, sampler):
    states = {e["state"]: t for t, e in events if e.get("type") == "state"}
    # The recording starts playing when the microphone opens, that is when loading starts.
    t0 = states["loading"]
    listening = states["idle"] - states["listening"]
    heard = [[] for _ in segments]
    for t, e in events:
        if e.get("type") in ("partial", "final"):
            at = t - t0
            k = max([i for i, s in enumerate(segments) if s["start"] - 0.3 <= at] or [0])
            heard[k].append((at, e["type"], e["text"]))

    first_partial, final_lag, clips = [], [], []
    for k, (segment, seen) in enumerate(zip(segments, heard)):
        partials = [(at, text) for at, kind, text in seen if kind == "partial"]
        finals = [(at, text) for at, kind, text in seen if kind == "final"]
        if k > 0 and partials:
            first_partial.append(round(partials[0][0] - segment["start"], 3))
        if k > 0 and finals:
            final_lag.append(round(finals[0][0] - segment["end"], 3))
        final = " ".join(text for _, text in finals)
        clips.append({
            "text": segment["text"],
            "final": final,
            "right": words(final) == words(segment["text"]),
            "foreign_final": bool(FOREIGN.search(final.lower())),
            "foreign_partials": sorted({text for _, text in partials if FOREIGN.search(text.lower())}),
        })
    return {
        "listening_s": round(listening, 2),
        "cpu_s": round(cpu, 2),
        "cpu_per_s": round(cpu / listening, 3),
        "engine_own_cpu_s": round(sum(sampler.own_cpu.values()), 2),
        "engines": len(sampler.own_cpu),
        "vram_peak_mib": sampler.vram_peak,
        "first_partial_s": first_partial,
        "final_after_speech_s": final_lag,
        "phrases": clips,
    }


def words(text):
    text = unicodedata.normalize("NFKD", text.lower().replace("'", " ")).encode("ascii", "ignore").decode()
    return re.sub(r"[^a-z ]", "", text).split()


def summary_line(r):
    wrong = sum(not c["right"] for c in r["phrases"])
    foreign = sum(c["foreign_final"] for c in r["phrases"])
    return (f"{r['label']} {r['recording']} #{r['run']}: cpu {r['cpu_s']} s ({r['cpu_per_s']}/s, engine "
            f"{r['engine_own_cpu_s']}), vram {r['vram_peak_mib']} MiB, {wrong} not right, {foreign} not French")


# ---------------------------------------------------------------------------------------------
# table


def table(work):
    rows = [json.loads(l) for l in open(work / "results.jsonl")]
    labels = list(dict.fromkeys(r["label"] for r in rows))
    print("| Setup | CPU/s short | CPU/s long | VRAM peak (MiB) | First partial (median) | Final after speech (median) | Clips not French | Clips with a non-French partial | Clips right |")
    print("|---|---|---|---|---|---|---|---|---|")
    for label in labels:
        mine = [r for r in rows if r["label"] == label]

        def per_s(name):
            v = [r["cpu_per_s"] for r in mine if r["recording"] == name]
            return " / ".join(f"{x:.3f}" for x in v) if v else "–"

        clips = [c for r in mine if r["recording"].startswith("clips-") for c in r["phrases"]]
        lags = [x for r in mine if r["recording"] != "long" for x in r["first_partial_s"]]
        finals = [x for r in mine if r["recording"] != "long" for x in r["final_after_speech_s"]]
        median = lambda v: f"{statistics.median(v):.2f} s" if v else "–"
        n = len(clips)
        print(f"| {label} | {per_s('short')} | {per_s('long')} | {max(r['vram_peak_mib'] for r in mine)} "
              f"| {median(lags)} | {median(finals)} "
              f"| {sum(c['foreign_final'] for c in clips)} / {n} "
              f"| {sum(bool(c['foreign_partials']) for c in clips)} / {n} "
              f"| {sum(c['right'] for c in clips)} / {n} |")


if __name__ == "__main__":
    main(sys.argv[1:])
