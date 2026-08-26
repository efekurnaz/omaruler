import math
import random
import struct
import wave

SR = 44100


def write_wav(path, samples):
    peak = max(0.0001, max(abs(s) for s in samples))
    scale = 0.9 / peak
    frames = b"".join(struct.pack("<h", int(max(-32767, min(32767, s * scale * 32767)))) for s in samples)
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(frames)


def env_exp_decay(t, tau):
    return math.exp(-t / tau)


def env_attack_decay(t, dur, attack=0.005, decay_tau=0.08):
    if t < attack:
        return t / attack
    return math.exp(-(t - attack) / decay_tau)


def gen_shot(dur=0.22):
    n = int(SR * dur)
    random.seed(1)
    out = []
    for i in range(n):
        t = i / SR
        noise = random.uniform(-1, 1)
        thump = math.sin(2 * math.pi * 70 * t) * 0.6
        env = env_exp_decay(t, 0.035)
        out.append((noise * 0.85 + thump) * env)
    return out


def sawtooth(f, t):
    x = t * f
    return 2 * (x - math.floor(0.5 + x))


def gen_quack(dur=0.22):
    n = int(SR * dur)
    out = []
    for i in range(n):
        t = i / SR
        frac = t / dur
        # quick downward pitch sweep, buzzy nasal tone
        f = 340 - 140 * frac
        v = sawtooth(f, t) * 0.6 + sawtooth(f * 2.01, t) * 0.25
        env = env_attack_decay(t, dur, attack=0.01, decay_tau=0.09)
        # slight amplitude wobble for a "wak-wak" texture
        wobble = 1.0 + 0.15 * math.sin(2 * math.pi * 28 * t)
        out.append(v * env * wobble)
    return out


def gen_fail(dur=0.72):
    n = int(SR * dur)
    notes = [392.0, 349.23, 293.66]  # G4, F4, D4 - "womp womp womp"
    note_dur = dur / len(notes)
    out = []
    for i in range(n):
        t = i / SR
        idx = min(len(notes) - 1, int(t / note_dur))
        f = notes[idx]
        local_t = t - idx * note_dur
        v = math.sin(2 * math.pi * f * local_t) * 0.7 + math.sin(2 * math.pi * f * 2 * local_t) * 0.2
        env = env_attack_decay(local_t, note_dur, attack=0.008, decay_tau=0.11)
        out.append(v * env)
    return out


def gen_chime(dur=0.5):
    n = int(SR * dur)
    notes = [523.25, 659.25, 784.0]  # C5, E5, G5
    note_dur = dur / len(notes)
    out = []
    for i in range(n):
        t = i / SR
        idx = min(len(notes) - 1, int(t / note_dur))
        f = notes[idx]
        local_t = t - idx * note_dur
        v = math.sin(2 * math.pi * f * local_t) * 0.65 + math.sin(2 * math.pi * f * 2 * local_t) * 0.25
        env = env_attack_decay(local_t, note_dur, attack=0.004, decay_tau=0.16)
        out.append(v * env)
    return out


write_wav("/home/efe/omaruler/assets/sounds/shot.wav", gen_shot())
write_wav("/home/efe/omaruler/assets/sounds/quack.wav", gen_quack())
write_wav("/home/efe/omaruler/assets/sounds/fail.wav", gen_fail())
write_wav("/home/efe/omaruler/assets/sounds/chime.wav", gen_chime())
print("done")
