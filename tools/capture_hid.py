#!/usr/bin/env python3
"""Summarize the Rust HID probe without saving card IDs or 1000 lines/second.

This observes actual inputs, but cannot infer physical sensor locations without
an operator following a known touch sequence. No HID writes are performed.
"""

import argparse
import collections
import json
import pathlib
import re
import subprocess
import sys
import time


RAW = re.compile(r"^raw len=(\d+) bytes=\[(.*)\]$")
ROOT = pathlib.Path(__file__).resolve().parents[1]


def parse_raw(line):
    match = RAW.fullmatch(line.strip())
    if match is None:
        return None
    # Rust {:02x?} emits bare hexadecimal tokens, not Python integer literals.
    payload = bytes(int(token.strip(), 16) for token in match[2].split(",") if token.strip())
    if len(payload) != int(match[1]):
        raise ValueError("raw output length does not match its declared length")
    return payload


def capture(binary, duration, max_samples):
    lengths = collections.Counter()
    air_values = collections.Counter()
    button_values = collections.Counter()
    peak = [0] * 32
    changed_offsets = set()
    first = None
    last = None
    previous = None
    active_frames = 0
    released_after_activity = False
    samples = []
    last_sample_time = -1.0
    started = time.monotonic()
    first_received = None
    last_received = None
    rust_summary = None
    command = [str(binary), "hid-read", "--reports", "0", "--duration-secs", str(duration), "--raw"]
    with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          stdin=subprocess.DEVNULL, text=True) as process:
        try:
            for line in process.stdout:
                raw = parse_raw(line)
                if raw is None:
                    if line.startswith("opened ") or line.startswith("summary "):
                        print(line.rstrip(), flush=True)
                    if line.startswith("summary "):
                        rust_summary = line.strip()
                    continue
                now = time.monotonic() - started
                lengths[len(raw)] += 1
                first_received = now if first_received is None else first_received
                last_received = now
                if len(raw) != 45:
                    continue
                if first is None:
                    first = raw
                changed_offsets.update(i for i, (a, b) in enumerate(zip(first, raw)) if a != b)
                air_values[raw[0]] += 1
                button_values[raw[1]] += 1
                slider = raw[2:34]
                active = any(slider)
                if active:
                    active_frames += 1
                elif active_frames:
                    released_after_activity = True
                peak = [max(a, b) for a, b in zip(peak, slider)]
                # Retain sparse transitions in the input fields only. Never save
                # bytes 34..44, which may contain a card type and identifier.
                if raw[:34] != previous and now - last_sample_time >= 0.25 and len(samples) < max_samples:
                    sample = {"elapsed_ms": round(now * 1000), "air": raw[0],
                              "buttons": raw[1], "slider_raw": list(slider)}
                    samples.append(sample)
                    last_sample_time = now
                previous = raw[:34]
                last = {"air": raw[0], "buttons": raw[1], "slider_raw": list(slider)}
            stderr = process.stderr.read()
            returncode = process.wait(timeout=3)
        except BaseException:
            process.kill()
            process.wait(timeout=3)
            raise
    if stderr:
        print(stderr, file=sys.stderr, end="")
    total = sum(lengths.values())
    report = {
        "kind": "real_hid_capture",
        "command": command,
        "elapsed_ms": round((time.monotonic() - started) * 1000),
        "returncode": returncode,
        "report_lengths": dict(sorted(lengths.items())),
        "total_reports": total,
        "first_received_ms": None if first_received is None else round(first_received * 1000),
        "last_received_ms": None if last_received is None else round(last_received * 1000),
        "changed_byte_offsets": sorted(changed_offsets),
        "pressure_active_frames": active_frames,
        "pressure_active_channels": [i for i, value in enumerate(peak) if value],
        "pressure_peak_raw": peak,
        "pressure_released_after_activity": released_after_activity,
        "air_values": dict(sorted(air_values.items())),
        "button_values": dict(sorted(button_values.items())),
        "samples": samples,
        "last_input": last,
        "rust_summary": rust_summary,
        "notes": [
            "No HID Output or Feature Report was sent.",
            "Card bytes 34..44 are not retained.",
            "Input rates are not game latency measurements or proof of no dropped USB reports.",
            "Physical channel positions and air/button semantics require a known operator sequence.",
        ],
    }
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, default=ROOT / "target/release/chuni-bridge-probe")
    parser.add_argument("--duration-secs", type=int, default=30)
    parser.add_argument("--max-samples", type=int, default=32)
    parser.add_argument("--output", type=pathlib.Path, default=ROOT / "artifacts/hid-capture.json")
    args = parser.parse_args()
    if args.duration_secs < 1 or args.max_samples < 0:
        parser.error("duration-secs must be positive; max-samples must be nonnegative")
    if not args.binary.is_file():
        parser.error("probe binary missing: run cargo build --locked --release")
    print(f"Capturing real HID for {args.duration_secs}s; touch the slider/air, then release.", flush=True)
    report = capture(args.binary.resolve(), args.duration_secs, args.max_samples)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    summary = {key: report[key] for key in (
        "returncode", "total_reports", "report_lengths", "changed_byte_offsets",
        "pressure_active_frames", "pressure_active_channels", "pressure_peak_raw",
        "pressure_released_after_activity", "air_values", "button_values", "last_input")}
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    print(f"Saved {args.output}")
    if report["returncode"] != 0 or report["total_reports"] == 0 or set(report["report_lengths"]) != {45}:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
