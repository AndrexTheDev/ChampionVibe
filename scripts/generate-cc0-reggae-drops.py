#!/usr/bin/env python3
"""Generates ChampionVibe's bundled Patois/reggae drop sampleboard.

Every file produced here is original synthesis written for ChampionVibe and is dedicated to the
public domain under CC0 1.0 (see src-tauri/assets/samples/LICENSE-CC0-1.0.txt). Nothing is sampled
from a recording, so no third-party master, performance, or vocal rights are involved. The vocal
drops are formant-synthesised shouts, not recordings of a singer; the repository documents that
plainly instead of implying a real Jamaican vocalist was sampled.

The generator is deterministic (fixed RNG seed), so anyone can rebuild the assets byte for byte
and verify the published hashes:

    python3 scripts/generate-cc0-reggae-drops.py            # write the .wav files
    python3 scripts/generate-cc0-reggae-drops.py --check    # verify committed files match

Output: src-tauri/assets/samples/*.wav (mono, 44.1 kHz, 16-bit PCM)
"""

from __future__ import annotations

import argparse
import math
import random
import struct
import sys
import wave
from pathlib import Path

SAMPLE_RATE = 44_100
OUTPUT_DIR = Path(__file__).resolve().parent.parent / "src-tauri" / "assets" / "samples"
SEED = 0x5EED_1975


class Rng:
    """Small deterministic RNG so the assets do not depend on Python's global seed."""

    def __init__(self, seed: int) -> None:
        self._rng = random.Random(seed)

    def uniform(self, low: float, high: float) -> float:
        return self._rng.uniform(low, high)

    def noise(self) -> float:
        return self._rng.uniform(-1.0, 1.0)


def make_buffer(seconds: float) -> list[float]:
    return [0.0] * int(round(seconds * SAMPLE_RATE))


def add_tone(
    buffer: list[float],
    start_seconds: float,
    seconds: float,
    frequency: float,
    amplitude: float,
    *,
    attack: float = 0.005,
    release: float = 0.08,
    detune: float = 0.0,
    harmonic: float = 0.0,
    pitch_sweep: float = 0.0,
) -> None:
    start = int(round(start_seconds * SAMPLE_RATE))
    length = int(round(seconds * SAMPLE_RATE))
    phase = 0.0
    for index in range(length):
        position = start + index
        if position >= len(buffer):
            break
        time = index / SAMPLE_RATE
        progress = index / max(1, length - 1)
        frequency_now = frequency * (1.0 + pitch_sweep * progress)
        phase += math.tau * frequency_now / SAMPLE_RATE
        envelope = min(1.0, time / max(attack, 1e-6)) * min(
            1.0, max(0.0, (seconds - time) / max(release, 1e-6))
        )
        value = math.sin(phase)
        if detune:
            value = 0.5 * (value + math.sin(phase * (1.0 + detune)))
        if harmonic:
            value += harmonic * math.sin(phase * 2.0)
        buffer[position] += value * amplitude * envelope


def add_noise_hit(
    buffer: list[float],
    start_seconds: float,
    seconds: float,
    amplitude: float,
    *,
    decay: float = 18.0,
    tone: float = 0.0,
    brightness: float = 1.0,
    rng: Rng | None = None,
) -> None:
    rng = rng or Rng(SEED)
    start = int(round(start_seconds * SAMPLE_RATE))
    length = int(round(seconds * SAMPLE_RATE))
    previous = 0.0
    for index in range(length):
        position = start + index
        if position >= len(buffer):
            break
        time = index / SAMPLE_RATE
        envelope = math.exp(-decay * time)
        noise = rng.noise()
        # One-pole low-pass shapes the hit; brightness 1.0 is raw noise, lower is darker.
        previous += (noise - previous) * min(1.0, brightness)
        body = math.sin(math.tau * tone * time) if tone else 0.0
        buffer[position] += (previous * 0.85 + body * 0.6) * amplitude * envelope


def add_kick(buffer: list[float], start_seconds: float, amplitude: float = 0.95) -> None:
    start = int(round(start_seconds * SAMPLE_RATE))
    length = int(round(0.42 * SAMPLE_RATE))
    phase = 0.0
    for index in range(length):
        position = start + index
        if position >= len(buffer):
            break
        time = index / SAMPLE_RATE
        frequency = 132.0 * math.exp(-26.0 * time) + 43.0
        phase += math.tau * frequency / SAMPLE_RATE
        envelope = math.exp(-7.5 * time)
        click = math.exp(-260.0 * time) * 0.35
        buffer[position] += (math.sin(phase) * envelope + click * 0.4) * amplitude


def add_snare(buffer: list[float], start_seconds: float, amplitude: float = 0.7, rng: Rng | None = None) -> None:
    add_noise_hit(buffer, start_seconds, 0.28, amplitude, decay=22.0, tone=196.0, brightness=0.75, rng=rng)


def add_hat(buffer: list[float], start_seconds: float, amplitude: float = 0.22, rng: Rng | None = None) -> None:
    add_noise_hit(buffer, start_seconds, 0.07, amplitude, decay=70.0, brightness=1.0, rng=rng)


def add_echo(buffer: list[float], delay_seconds: float, feedback: float, repeats: int = 3) -> None:
    delay = int(round(delay_seconds * SAMPLE_RATE))
    if delay <= 0:
        return
    tail = buffer[:]
    for repeat in range(1, repeats + 1):
        gain = feedback ** repeat
        offset = delay * repeat
        for index in range(min(len(buffer), len(tail) - offset)):
            buffer[index + offset] += tail[index] * gain


def add_room(buffer: list[float], amount: float = 0.18) -> None:
    """A tiny comb/allpass room so one-shots do not sound bone dry."""
    taps = (0.011, 0.019, 0.029, 0.041)
    for tap in taps:
        delay = int(round(tap * SAMPLE_RATE))
        for index in range(delay, len(buffer)):
            buffer[index] += buffer[index - delay] * amount / len(taps)


def formant_shout(
    buffer: list[float],
    start_seconds: float,
    seconds: float,
    formants: tuple[float, ...],
    f0_start: float,
    f0_end: float,
    amplitude: float,
) -> None:
    """Formant-synthesised shout: glottal pulse train shaped by resonant bands."""
    start = int(round(start_seconds * SAMPLE_RATE))
    length = int(round(seconds * SAMPLE_RATE))
    phase = 0.0
    bands = [
        [0.0, 0.0] for _ in formants
    ]  # two-pole resonator state per formant band
    for index in range(length):
        position = start + index
        if position >= len(buffer):
            break
        progress = index / max(1, length - 1)
        time = index / SAMPLE_RATE
        f0 = f0_start + (f0_end - f0_start) * progress
        phase += f0 / SAMPLE_RATE
        if phase >= 1.0:
            phase -= 1.0
        # Band-limited-ish glottal pulse.
        pulse = math.sin(math.tau * phase) ** 3 if phase < 0.5 else 0.0
        value = 0.0
        for band_index, formant in enumerate(formants):
            state = bands[band_index]
            state[0] += (pulse - state[0] - 0.35 * state[1]) * min(1.0, formant / SAMPLE_RATE * 6.0)
            state[1] += state[0] * 0.35 * 2.0 * math.pi * formant / SAMPLE_RATE
            value += state[0]
        envelope = min(1.0, time / 0.02) * min(1.0, max(0.0, (seconds - time) / 0.09))
        buffer[position] += value * amplitude * envelope


def soften(buffer: list[float], drive: float = 1.4) -> None:
    for index, value in enumerate(buffer):
        buffer[index] = math.tanh(value * drive) / math.tanh(drive)


def normalize(buffer: list[float], target: float = 0.86) -> None:
    peak = max((abs(value) for value in buffer), default=0.0)
    if peak <= 1e-9:
        return
    scale = target / peak
    for index, value in enumerate(buffer):
        buffer[index] = value * scale


def write_wav(path: Path, buffer: list[float]) -> None:
    frames = bytearray()
    for value in buffer:
        clamped = max(-1.0, min(1.0, value))
        frames += struct.pack("<h", int(round(clamped * 32767.0)))
    with wave.open(str(path), "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(SAMPLE_RATE)
        handle.writeframes(bytes(frames))


def airhorn_classic() -> list[float]:
    buffer = make_buffer(1.1)
    rng = Rng(SEED + 1)
    # The three-note airhorn figure that opens countless sound-system sets.
    for offset, frequency in ((0.0, 392.0), (0.22, 523.25), (0.44, 659.25)):
        add_tone(buffer, offset, 0.56, frequency, 0.5, detune=0.012, harmonic=0.22)
        add_tone(buffer, offset, 0.56, frequency * 2.0, 0.12, detune=-0.008)
    add_noise_hit(buffer, 0.0, 0.08, 0.10, decay=40.0, brightness=0.6, rng=rng)
    add_room(buffer, 0.14)
    soften(buffer, 1.6)
    return buffer


def siren_ragga() -> list[float]:
    buffer = make_buffer(1.6)
    notes = ((0.0, 1180.0), (0.34, 880.0), (0.68, 660.0), (1.02, 880.0))
    for offset, frequency in notes:
        add_tone(buffer, offset, 0.34, frequency, 0.52, attack=0.004, release=0.02, harmonic=0.3)
    add_echo(buffer, 0.17, 0.42, repeats=3)
    soften(buffer, 1.3)
    return buffer


def siren_dub_sweep() -> list[float]:
    buffer = make_buffer(2.0)
    rng = Rng(SEED + 2)
    add_tone(buffer, 0.0, 2.0, 520.0, 0.5, attack=0.05, release=0.25, pitch_sweep=0.85, harmonic=0.35)
    add_tone(buffer, 0.0, 2.0, 520.0 * 1.505, 0.18, attack=0.05, release=0.25, pitch_sweep=0.85)
    # Slow delay wobble that gives dub sirens their drifting tail.
    for index in range(len(buffer)):
        time = index / SAMPLE_RATE
        buffer[index] *= 1.0 + 0.12 * math.sin(math.tau * 5.5 * time)
    add_noise_hit(buffer, 0.0, 0.3, 0.05, decay=12.0, brightness=0.35, rng=rng)
    add_echo(buffer, 0.24, 0.55, repeats=4)
    soften(buffer, 1.35)
    return buffer


def drum_one_drop() -> list[float]:
    buffer = make_buffer(2.0)
    rng = Rng(SEED + 3)
    bar = 0.48  # ~125 BPM dancehall half-bar grid
    for step in range(4):
        offset = step * bar
        add_kick(buffer, offset + 0.24, 0.92)  # one drop: kick lands with the snare on beat three
        add_snare(buffer, offset + 0.24, 0.72, rng)
        for offbeat in (0.06, 0.30, 0.42):
            add_hat(buffer, offset + offbeat, 0.18, rng)
    add_room(buffer, 0.12)
    return buffer


def drum_steppers() -> list[float]:
    buffer = make_buffer(2.0)
    rng = Rng(SEED + 4)
    beat = 0.24
    for step in range(8):
        offset = step * beat
        add_kick(buffer, offset, 0.88)  # steppers: four-on-the-floor kick
        if step % 4 == 2:
            add_snare(buffer, offset, 0.68, rng)
        add_hat(buffer, offset + beat / 2.0, 0.16, rng)
    add_room(buffer, 0.1)
    return buffer


def skank_chord() -> list[float]:
    buffer = make_buffer(1.2)
    chord = (261.63, 329.63, 392.0, 493.88)
    for offset in (0.0, 0.3, 0.6, 0.9):
        for frequency in chord:
            add_tone(buffer, offset, 0.2, frequency, 0.2, attack=0.002, release=0.06, harmonic=0.15)
    add_echo(buffer, 0.15, 0.3, repeats=2)
    soften(buffer, 1.4)
    return buffer


def bass_riddim() -> list[float]:
    buffer = make_buffer(2.4)
    pattern = (
        (0.0, 65.41, 0.28),
        (0.3, 65.41, 0.18),
        (0.6, 87.31, 0.26),
        (0.9, 65.41, 0.18),
        (1.2, 98.0, 0.3),
        (1.5, 87.31, 0.18),
        (1.8, 73.42, 0.26),
        (2.1, 65.41, 0.26),
    )
    for offset, frequency, duration in pattern:
        add_tone(buffer, offset, duration, frequency, 0.62, attack=0.008, release=0.05, harmonic=0.4)
    soften(buffer, 1.5)
    return buffer


def fx_tape_rewind() -> list[float]:
    buffer = make_buffer(1.1)
    rng = Rng(SEED + 5)
    length = len(buffer)
    phase = 0.0
    for index in range(length):
        progress = index / length
        time = index / SAMPLE_RATE
        frequency = (1500.0 - 1250.0 * progress) * (1.0 + 0.05 * math.sin(math.tau * 9.0 * time))
        phase += math.tau * frequency / SAMPLE_RATE
        envelope = (1.0 - progress) ** 1.5
        flutter = math.sin(phase) * 0.55 + rng.noise() * 0.18
        buffer[index] = flutter * envelope * 0.7
    add_room(buffer, 0.16)
    soften(buffer, 1.4)
    return buffer


def vocal_hey() -> list[float]:
    buffer = make_buffer(0.75)
    formant_shout(buffer, 0.0, 0.42, (720.0, 1180.0, 2540.0), 232.0, 178.0, 0.85)
    add_echo(buffer, 0.14, 0.3, repeats=2)
    soften(buffer, 1.5)
    return buffer


def vocal_pull_up() -> list[float]:
    buffer = make_buffer(1.35)
    formant_shout(buffer, 0.0, 0.34, (520.0, 980.0, 2380.0), 168.0, 150.0, 0.85)  # "pull"
    formant_shout(buffer, 0.42, 0.52, (760.0, 1250.0, 2450.0), 246.0, 168.0, 0.85)  # "up"
    add_echo(buffer, 0.19, 0.34, repeats=3)
    soften(buffer, 1.45)
    return buffer


DROPS: dict[str, callable] = {
    "airhorn-classic.wav": airhorn_classic,
    "siren-ragga.wav": siren_ragga,
    "siren-dub-sweep.wav": siren_dub_sweep,
    "drum-one-drop.wav": drum_one_drop,
    "drum-steppers.wav": drum_steppers,
    "skank-chord.wav": skank_chord,
    "bass-riddim.wav": bass_riddim,
    "fx-tape-rewind.wav": fx_tape_rewind,
    "vocal-hey.wav": vocal_hey,
    "vocal-pull-up.wav": vocal_pull_up,
}


def render(name: str, builder) -> list[float]:
    buffer = builder()
    normalize(buffer)
    return buffer


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="Verify that the committed .wav files match freshly rendered output.",
    )
    arguments = parser.parse_args()

    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    mismatches = 0
    for name, builder in sorted(DROPS.items()):
        path = OUTPUT_DIR / name
        audio = render(name, builder)
        if arguments.check:
            bytes_expected = expected_bytes(audio)
            if not path.is_file():
                print(f"missing {path}")
                mismatches += 1
                continue
            if path.read_bytes() != bytes_expected:
                print(f"differs {path}")
                mismatches += 1
            else:
                print(f"ok      {path.relative_to(OUTPUT_DIR.parent.parent.parent)}")
            continue
        write_wav(path, audio)
        print(f"wrote {path.name} ({len(audio) / SAMPLE_RATE:.2f}s)")

    if arguments.check:
        if mismatches:
            print(f"{mismatches} sample(s) differ from the generator output.", file=sys.stderr)
            return 1
        print("All sampleboard assets match the generator output.")
    return 0


def expected_bytes(buffer: list[float]) -> bytes:
    frames = bytearray()
    for value in buffer:
        clamped = max(-1.0, min(1.0, value))
        frames += struct.pack("<h", int(round(clamped * 32767.0)))
    header = bytearray()
    data_size = len(frames)
    header += b"RIFF" + struct.pack("<I", 36 + data_size) + b"WAVE"
    header += b"fmt " + struct.pack("<IHHIIHH", 16, 1, 1, SAMPLE_RATE, SAMPLE_RATE * 2, 2, 16)
    header += b"data" + struct.pack("<I", data_size)
    return bytes(header + frames)


if __name__ == "__main__":
    sys.exit(main())
