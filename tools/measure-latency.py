#!/usr/bin/env python3
"""Measure the round-trip latency of whatever is wired between an output and an input.

    tools/measure-latency.py --out plughw:1,0 --in plughw:1,0

The script does not know or care what sits in the loop. It emits a chirp on
the output, listens on the input, and reports how long the signal took to come
back. What that number *means* is decided by how you wire it:

  out ──cable──► in                    the hardware floor: ALSA buffers, the
  (same interface)                     DAC, the ADC. Measure this first; every
                                       other number is only meaningful once
                                       you can subtract it.

  out ──cable──► [SoundNet in]         the same floor plus one trip through
       ...network...                   SoundNet. Wire the far end's output
  [SoundNet out] ──cable──► in         back into this machine's input.

Why one process: playback and capture must share a time base. Running `aplay`
and `arecord` separately and diffing their timestamps measures nothing — there
is no common clock between two processes, and the answer is whatever the
scheduler felt like that day. Here both streams are driven from a single
lock-step loop (read one period, write one period), so a sample index in the
capture stream is directly comparable to one in the playback stream.

Requires: python3-alsaaudio, python3-numpy
    sudo apt install python3-alsaaudio python3-numpy
"""

import argparse
import sys

try:
    import numpy as np
except ImportError:
    sys.exit("needs numpy: sudo apt install python3-numpy")
try:
    import alsaaudio
except ImportError:
    sys.exit("needs alsaaudio: sudo apt install python3-alsaaudio")


# Formats to try, best first. The engine has the same problem and solves it the
# same way (see audio/format.rs): ALSA has no "nearest" fallback for format, so
# a raw hw: device that wants S24_3LE simply refuses S16_LE, and a measurement
# tool that only speaks one format cannot measure half the interfaces it is
# pointed at.
FORMATS = [
    ("S32_LE", alsaaudio.PCM_FORMAT_S32_LE, 4),
    ("S16_LE", alsaaudio.PCM_FORMAT_S16_LE, 2),
    ("S24_3LE", alsaaudio.PCM_FORMAT_S24_3LE, 3),
]


def open_pcm(direction, device, rate, channels, period, fmt):
    return alsaaudio.PCM(
        type=direction,
        mode=alsaaudio.PCM_NORMAL,
        device=device,
        channels=channels,
        rate=rate,
        format=fmt,
        periodsize=period,
    )


def negotiate(out_dev, in_dev, rate, channels, period):
    """Open both devices with the first format they both accept."""
    errors = []
    for name, fmt, width in FORMATS:
        try:
            play = open_pcm(alsaaudio.PCM_PLAYBACK, out_dev, rate, channels, period, fmt)
            rec = open_pcm(alsaaudio.PCM_CAPTURE, in_dev, rate, channels, period, fmt)
            return play, rec, name, width
        except alsaaudio.ALSAAudioError as err:
            errors.append(f"  {name}: {err}")
    sys.exit("no format worked on both devices:\n" + "\n".join(errors))


def to_bytes(samples, width, channels):
    """Interleave one mono float array to `channels` and encode for ALSA."""
    clipped = np.clip(samples, -1.0, 1.0)
    frame = np.repeat(clipped[:, None], channels, axis=1).ravel()
    if width == 4:
        return (frame * 2147483000.0).astype("<i4").tobytes()
    if width == 2:
        return (frame * 32767.0).astype("<i2").tobytes()
    # S24_3LE: take the top three bytes of a 32-bit little-endian sample.
    packed = (frame * 8388607.0).astype("<i4").view(np.uint8).reshape(-1, 4)
    return packed[:, :3].tobytes()


def from_bytes(raw, width, channels):
    """Decode ALSA bytes and return channel 0 as floats."""
    if width == 4:
        mono = np.frombuffer(raw, dtype="<i4")[0::channels] / 2147483648.0
    elif width == 2:
        mono = np.frombuffer(raw, dtype="<i2")[0::channels] / 32768.0
    else:
        b = np.frombuffer(raw, dtype=np.uint8).reshape(-1, 3)
        # Sign-extend 24 -> 32 by putting the bytes in the high three octets.
        wide = np.zeros((len(b), 4), dtype=np.uint8)
        wide[:, 1:] = b
        mono = wide.view("<i4").ravel()[0::channels] / 2147483648.0
    return mono.astype(np.float32)


def chirp(rate, ms, f0, f1):
    """A short linear sweep.

    A single click would do in a quiet room, but a sweep survives noise and a
    modest input level: cross-correlating against a known sweep concentrates
    all of its energy into one sharp peak, so the detector still works when the
    click itself is buried. That matters here — the usual reason a loopback
    measurement "doesn't detect anything" is that the return is quieter than
    expected, not that it is absent.
    """
    n = int(rate * ms / 1000)
    t = np.arange(n) / rate
    sweep = np.sin(2 * np.pi * (f0 * t + (f1 - f0) / (2 * (n / rate)) * t * t))
    # Taper the ends so the sweep starts and stops without a step, which would
    # itself ring and blur the correlation peak.
    window = np.hanning(n)
    return (sweep * window).astype(np.float32)


def measure_once(play, rec, rate, channels, period, width, probe, listen_frames, settle_frames):
    """Emit `probe` once and return (capture samples, index where it was emitted)."""
    silence = to_bytes(np.zeros(period, dtype=np.float32), width, channels)

    # Let the loop stabilise before the probe goes out. The first moments after
    # a device opens are not representative: the driver is still settling, and
    # many interfaces emit an audible pop that a naive peak detector will
    # happily report as "the signal", giving an absurdly short latency. (That
    # is exactly what a peak at sample 59 meant in an earlier attempt.)
    for _ in range(settle_frames // period):
        play.write(silence)
        rec.read()

    captured = []
    emitted_at = None
    probe_queue = list(probe)
    written = 0

    total_iters = (listen_frames // period) + 1
    for i in range(total_iters):
        # Write one period: the probe first, silence afterwards.
        if probe_queue:
            chunk = np.array(probe_queue[:period], dtype=np.float32)
            probe_queue = probe_queue[period:]
            if len(chunk) < period:
                chunk = np.concatenate([chunk, np.zeros(period - len(chunk), np.float32)])
            if emitted_at is None:
                emitted_at = written
            play.write(to_bytes(chunk, width, channels))
        else:
            play.write(silence)
        written += period

        length, data = rec.read()
        if length > 0:
            captured.append(from_bytes(data, width, channels))

    return np.concatenate(captured) if captured else np.zeros(0, np.float32), emitted_at


def locate(captured, probe, rate):
    """Cross-correlate and return (delay_frames, peak, noise_floor)."""
    if len(captured) < len(probe):
        return None, 0.0, 0.0
    corr = np.correlate(captured, probe, mode="valid")
    peak_idx = int(np.argmax(np.abs(corr)))
    peak = float(np.abs(corr[peak_idx]))
    # Everything outside a window around the peak is "not the signal", and its
    # RMS is what the peak has to stand out from. Reporting that ratio is the
    # difference between "12.3 ms" and "12.3 ms, and here is why you should
    # believe it".
    guard = len(probe)
    mask = np.ones(len(corr), dtype=bool)
    mask[max(0, peak_idx - guard) : peak_idx + guard] = False
    noise = float(np.sqrt(np.mean(corr[mask] ** 2))) if mask.any() else 0.0
    return peak_idx, peak, noise


def main():
    ap = argparse.ArgumentParser(
        description="Measure round-trip audio latency through a physical loop.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__.split("Requires:")[0],
    )
    ap.add_argument("--out", required=True, help="playback device, e.g. plughw:1,0")
    ap.add_argument("--in", dest="inp", required=True, help="capture device")
    ap.add_argument("--rate", type=int, default=48000)
    ap.add_argument("--channels", type=int, default=2)
    ap.add_argument("--period", type=int, default=256,
                    help="ALSA period in frames. Also the resolution of the "
                         "result: start/stop skew between the two streams is "
                         "bounded by one period (default 256 = 5.3 ms at 48k). "
                         "Use 64 for a tighter figure on a device that can "
                         "take it.")
    ap.add_argument("--repeat", type=int, default=10, help="measurements to take")
    ap.add_argument("--max-ms", type=float, default=500.0,
                    help="how long to listen after the probe")
    args = ap.parse_args()

    play, rec, fmt_name, width = negotiate(
        args.out, args.inp, args.rate, args.channels, args.period
    )
    print(f"format {fmt_name}, {args.rate} Hz, {args.channels} ch, period {args.period}")
    print(f"out {args.out}  ->  in {args.inp}")

    probe = chirp(args.rate, ms=8, f0=500, f1=8000)
    listen = int(args.rate * args.max_ms / 1000)
    settle = args.rate // 2  # 500 ms

    results = []
    for n in range(args.repeat):
        captured, emitted_at = measure_once(
            play, rec, args.rate, args.channels, args.period, width, probe, listen, settle
        )
        idx, peak, noise = locate(captured, probe, args.rate)
        level = float(np.max(np.abs(captured))) if len(captured) else 0.0
        if idx is None or noise == 0 or peak < noise * 8:
            print(f"  [{n + 1}/{args.repeat}] no signal "
                  f"(peak/noise {peak / noise:.1f}x)" if noise else
                  f"  [{n + 1}/{args.repeat}] no signal")
            print(f"      loudest sample in the recording: {level:.4f} "
                  f"({20 * np.log10(level):.0f} dBFS)" if level > 0 else
                  "      the recording is digital silence")
            continue
        # `emitted_at` is the playback-stream index where the probe began;
        # `idx` is the capture-stream index where it came back. Both streams
        # advance together in the loop above, so the difference is the
        # round trip.
        frames = idx - emitted_at
        ms = frames * 1000.0 / args.rate
        results.append(ms)
        print(f"  [{n + 1}/{args.repeat}] {ms:8.2f} ms   "
              f"({frames} frames, peak/noise {peak / noise:5.1f}x, "
              f"level {20 * np.log10(level):.0f} dBFS)")

    print()
    if not results:
        print("Nothing was detected. In order of likelihood:")
        print("  - the cable is not actually connecting this output to this input")
        print("  - the input is muted or its gain is at zero (alsamixer -c <card>)")
        print("  - the interface has a routing switch that decides what reaches")
        print("    the computer (on a Yamaha AG06, TO PC must be LOOPBACK, not DRY CH 1-2)")
        print("  - the output is muted, or feeding a different physical jack")
        print("Run `alsamixer -c <card>` and watch the input meter while playing")
        print("something loud; if the meter does not move, no software can help.")
        sys.exit(1)

    arr = np.array(results)
    print(f"round trip: median {np.median(arr):.2f} ms   "
          f"min {arr.min():.2f}   max {arr.max():.2f}   "
          f"spread {arr.max() - arr.min():.2f} ms   (n={len(arr)})")
    print()
    print(f"Resolution is one period ({args.period * 1000.0 / args.rate:.2f} ms): the two")
    print("streams are started as close together as the API allows, not sample-locked,")
    print("so treat that as the error bar. A spread much larger than one period means")
    print("something in the loop is not running at a steady rate — look at xruns.")


if __name__ == "__main__":
    main()
