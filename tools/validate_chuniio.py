#!/usr/bin/env python3
"""Load both real chuniio DLLs in Wine, exercise the game-facing ABI and the
LED publications visible in the shared-memory LED region."""

import argparse
import contextlib
import json
import mmap
import os
import pathlib
import queue
import re
import struct
import subprocess
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]

# Shared-memory LED region (protocol v2); see docs/probe-protocol.md.
LED_SEQ, LED_GEN, LED_GROUND, LED_AIR_LEFT, LED_AIR_RIGHT = 32, 33, 34, 58, 59
# Fixed air colors emitted by tools/chuniio_probe.c led_frame().
LED_AIR_LEFT_COLOR = bytes((0x11, 0x22, 0x33))
LED_AIR_RIGHT_COLOR = bytes((0xAA, 0xBB, 0xCC))


def read_led_region(region):
    """Seqlock read of the LED region; returns None while busy (odd seq)."""
    for _ in range(3):
        (seq,) = struct.unpack_from("<I", region, LED_SEQ * 4)
        if seq & 1:
            continue
        (gen,) = struct.unpack_from("<I", region, LED_GEN * 4)
        ground = bytes(region[LED_GROUND * 4:LED_GROUND * 4 + 96])
        air_left = bytes(region[LED_AIR_LEFT * 4:LED_AIR_LEFT * 4 + 3])
        air_right = bytes(region[LED_AIR_RIGHT * 4:LED_AIR_RIGHT * 4 + 3])
        if seq == struct.unpack_from("<I", region, LED_SEQ * 4)[0]:
            return {"seq": seq, "gen": gen, "ground": ground,
                    "air_left": air_left, "air_right": air_right}
    return None


def led_pattern_ok(state):
    """Probe sends a raw GBR gradient; require device BRG (source [1,2,0]).
    Recover its base from ground[2], the original first G byte."""
    ground = state["ground"]
    return (len(ground) == 96
            and all(ground[i:i + 3] == bytes(((ground[2] + i + 1) & 0xFF,
                                            (ground[2] + i + 2) & 0xFF,
                                            (ground[2] + i) & 0xFF))
                    for i in range(0, 96, 3))
            and state["air_left"] == LED_AIR_LEFT_COLOR
            and state["air_right"] == LED_AIR_RIGHT_COLOR)


def observe_led_publications(path, timeout=8.0, interval=0.05):
    """Observe the LED region while the Wine probe publishes it: require two
    coherent publications with an increasing generation and a valid pattern."""
    deadline = time.monotonic() + timeout
    first = None
    while time.monotonic() < deadline:
        try:
            fd = os.open(path, os.O_RDONLY)
        except FileNotFoundError:
            time.sleep(0.1)
            continue
        try:
            if os.fstat(fd).st_size != 4096:
                time.sleep(0.1)
                continue
            with mmap.mmap(fd, 4096, access=mmap.ACCESS_READ) as region:
                while time.monotonic() < deadline:
                    state = read_led_region(region)
                    if state and state["gen"] >= 1:
                        if not led_pattern_ok(state):
                            raise RuntimeError(f"LED region payload mismatch: gen={state['gen']} "
                                               f"air_left={state['air_left'].hex()} "
                                               f"air_right={state['air_right'].hex()}")
                        if first is None:
                            first = state["gen"]
                        elif state["gen"] > first:
                            return {"ok": True, "first_gen": first, "latest_gen": state["gen"]}
                    time.sleep(interval)
        finally:
            os.close(fd)
    raise RuntimeError("no advancing LED publications observed in the shared region")


def validate(wine):
    prefix = ROOT / "artifacts/wine-prefix"
    if not (prefix / "system.reg").is_file():
        raise RuntimeError("initialize the isolated artifacts/wine-prefix first")
    run = pathlib.Path(tempfile.mkdtemp(prefix="chuniio-", dir=ROOT / "artifacts"))
    env = dict(os.environ, WINEPREFIX=str(prefix), WINEDEBUG="-all",
               WINEDLLOVERRIDES="mscoree,mshtml=")
    events = queue.Queue()
    children, threads, observations = [], [], []
    producer_logs = []

    def collect(bits, child, log):
        for line in child.stdout:
            line = line.rstrip()
            log.write(line + "\n")
            log.flush()
            record = {"bits": bits, "line": line, "time": time.monotonic()}
            record.update({key: int(value) for key, value in re.findall(r"\b(\w+)=(\d+)\b", line)})
            match = re.search(r"\bstate=(\w+)", line)
            if match:
                record["state"] = match[1]
                record["slider"] = json.loads(line.split("slider=", 1)[1])
            events.put(record)

    def wait_both(predicate, after=0, timeout=8):
        found = {}
        deadline = time.monotonic() + timeout
        while len(found) < 2:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError(f"timed out waiting for DLL consumers; matched={list(found)}")
            try:
                record = events.get(timeout=remaining)
            except queue.Empty as error:
                raise RuntimeError("DLL consumer stopped producing output") from error
            observations.append(record)
            if record.get("bad_callbacks", 0) or record["line"].startswith("error="):
                raise RuntimeError(record["line"])
            if record["time"] >= after and predicate(record):
                found[record["bits"]] = record
        return found

    def producer(path, cleanup):
        log_path = run / f"producer{len(producer_logs)}.log"
        producer_logs.append(log_path)
        log = cleanup.enter_context(log_path.open("w"))
        child = subprocess.Popen([str(ROOT / "target/release/chuni-bridge-probe"),
                                  "produce", "--path", str(path), "--duration-secs", "20"],
                                 stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
        children.append(child)
        return child

    def stop_children():
        for child in children:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=5)
        for thread in threads:
            thread.join(timeout=3)

    with tempfile.TemporaryDirectory(prefix="chuni-dll-", dir="/dev/shm") as directory:
        with contextlib.ExitStack() as cleanup, contextlib.ExitStack() as processes_cleanup:
            processes_cleanup.callback(stop_children)
            path = pathlib.Path(directory) / "input"
            ini = run / "segatools.ini"
            ini.write_text("[io3]\ntest=0\nservice=0\ncoin=0\n")
            env["CHUNI_BRIDGE_PATH"] = "Z:" + str(path).replace("/", "\\")
            env["CHUNI_BRIDGE_INI"] = "Z:" + str(ini).replace("/", "\\")
            readers = {}
            for bits, arch in ((32, "x86"), (64, "x64")):
                out = ROOT / "target/chuniio" / arch
                log = cleanup.enter_context((run / f"probe{bits}.log").open("w"))
                err = cleanup.enter_context((run / f"probe{bits}-stderr.log").open("w"))
                # Only one probe may exercise the LED exports: two processes
                # would race the LED region seqlock (a game is one process).
                extra = [] if bits == 32 else ["--no-led"]
                child = subprocess.Popen([str(wine), str(out / "chuniio-probe.exe"),
                                          "--dll", "Z:" + str(out / "chuniio.dll").replace("/", "\\")] + extra,
                                         env=env, stdin=subprocess.DEVNULL,
                                         stdout=subprocess.PIPE, stderr=err, text=True)
                children.append(child)
                readers[bits] = child
                thread = threading.Thread(target=collect, args=(bits, child, log), daemon=True)
                thread.start()
                threads.append(thread)
            try:
                wait_both(lambda r: r.get("state") == "zero" and r.get("callbacks", 0) > 0)
                child = producer(path, cleanup)
                wait_both(lambda r: r.get("state") == "active")
                # The 32-bit probe is exercising the LED exports concurrently.
                led_result = observe_led_publications(path)
                inode = path.stat().st_ino
                wait_both(lambda r: r.get("lifecycle_restart") == 1)
                killed = time.monotonic()
                child.kill()
                if child.wait(timeout=3) != -9:
                    raise RuntimeError("producer was not SIGKILLed")
                stale = wait_both(lambda r: r.get("state") == "zero", after=killed)
                delays = {bits: round((r["time"] - killed) * 1000) for bits, r in stale.items()}
                if not all(0 <= ms <= 1500 for ms in delays.values()):
                    raise RuntimeError(f"unexpected stale-zero observations: {delays}")
                resumed_at = time.monotonic()
                child = producer(path, cleanup)
                wait_both(lambda r: r.get("state") == "active", after=resumed_at)
                if path.stat().st_ino != inode:
                    raise RuntimeError("producer replaced the input inode")
                stopped = time.monotonic()
                child.terminate()
                if child.wait(timeout=3) != 0:
                    raise RuntimeError("producer failed normal shutdown")
                wait_both(lambda r: r.get("state") == "zero", after=stopped)
                for bits, reader in readers.items():
                    if reader.wait(timeout=15) != 0:
                        raise RuntimeError(f"DLL consumer {bits} failed: inspect {run}")
                for thread in threads:
                    thread.join(timeout=3)
                while not events.empty():
                    observations.append(events.get_nowait())
                summaries = {r["bits"]: r for r in observations if r["line"].startswith("summary ")}
                if set(summaries) != {32, 64}:
                    raise RuntimeError("missing consumer summaries")
                led_counts = {bits: int(re.search(r"led_frames=(\d+)", r["line"])[1])
                              for bits, r in summaries.items()}
                if led_counts[32] <= 0 or led_counts[64] != 0:
                    raise RuntimeError(f"unexpected LED frame counts: {led_counts}")
                sessions = [int(re.search(r"session=(\d+)", p.read_text())[1]) for p in producer_logs]
                if sessions[0] == sessions[1]:
                    raise RuntimeError("producer session did not change")
                report = {"result": "PASS", "directory": str(run), "same_inode": True,
                          "producer_sessions": sessions, "sigkill_to_zero_sample_ms": delays,
                          "led": led_result, "led_frames": led_counts,
                          "reader_returncodes": {bits: p.returncode for bits, p in readers.items()},
                          "summaries": summaries, "observations": observations}
            except BaseException as error:
                report = {"result": "FAIL", "error": f"{type(error).__name__}: {error}",
                          "directory": str(run), "observations": observations}
                (run / "result.json").write_text(json.dumps(report, indent=2) + "\n")
                raise
    report["temporary_mapping_removed"] = not path.exists()
    (run / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k != "observations"}, indent=2))
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wine", type=pathlib.Path, required=True)
    args = parser.parse_args()
    validate(args.wine.resolve())


if __name__ == "__main__":
    main()
