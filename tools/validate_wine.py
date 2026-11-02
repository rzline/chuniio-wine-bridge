#!/usr/bin/env python3
"""Exercise actual 32/64-bit PE readers against one native Linux producer.

Uses only artifacts/wine-prefix (never the default/game prefix). Initialize it
with this runner's wineboot and build both PE probes before running this tool.
"""

import argparse
import contextlib
import json
import os
import pathlib
import queue
import re
import subprocess
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
STATE = re.compile(r"^state=(\w+)")


def validate(wine):
    artifacts = ROOT / "artifacts"
    prefix = artifacts / "wine-prefix"
    if not (prefix / "system.reg").is_file():
        raise RuntimeError("initialize artifacts/wine-prefix with the chosen wineboot first")
    env = dict(os.environ, WINEPREFIX=str(prefix), WINEDEBUG="-all",
               WINEDLLOVERRIDES="mscoree,mshtml=")
    events = queue.Queue()
    processes = []
    handles = []
    threads = []
    observations = []
    started = time.monotonic()
    producer = None

    def collect(bits, process, log):
        for line in process.stdout:
            line = line.rstrip()
            log.write(line + "\n")
            log.flush()
            events.put((time.monotonic(), bits, line))

    def wait_both(predicate, timeout=6):
        matched = {}
        deadline = time.monotonic() + timeout
        while len(matched) != 2:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(f"timed out waiting for both PE readers; matched={matched}")
            try:
                timestamp, bits, line = events.get(timeout=remaining)
            except queue.Empty as error:
                raise AssertionError(f"no further Wine output; matched={matched}") from error
            match = STATE.match(line)
            if match:
                state = match[1]
                record = {"at_ms": round((timestamp - started) * 1000), "bits": bits,
                          "state": state}
                for key in ("session", "heartbeat"):
                    value = re.search(rf"\b{key}=(\d+)", line)
                    if value:
                        record[key] = int(value[1])
                observations.append(record)
                # Every non-live observation must have safe inputs, not just a
                # state label hiding previously pressed synthetic pressure.
                if state != "live":
                    slider = line.split("slider=", 1)[1]
                    if slider != "[0;32]":
                        assert json.loads(slider) == [0] * 32, line
                    for key in ("air", "buttons"):
                        value = re.search(rf"\b{key}=(\d+)", line)
                        assert value is None or value[1] == "0", line
                if predicate(record):
                    matched[bits] = record
            if line.startswith("error="):
                raise AssertionError(f"PE{bits}: {line}")
        return matched

    def start_producer(path, duration):
        log = (artifacts / f"wine-producer-{len(processes)}.log").open("w")
        handles.append(log)
        child = subprocess.Popen([str(ROOT / "target/release/chuni-bridge-probe"),
                                  "produce", "--path", str(path),
                                  "--duration-secs", str(duration)],
                                 stdout=log, stderr=subprocess.STDOUT)
        processes.append(child)
        return child

    def stop_children():
        for child in processes:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=3)
        for thread in threads:
            thread.join(timeout=2)
        for handle in handles:
            handle.close()

    try:
        with tempfile.TemporaryDirectory(prefix="chuni-wine-probe-", dir="/dev/shm") as directory, contextlib.ExitStack() as cleanup:
            # ExitStack runs before TemporaryDirectory, including on failures:
            # stop all mappings before removing the backing file.
            cleanup.callback(stop_children)
            path = pathlib.Path(directory) / "input"
            dos_path = "Z:" + str(path).replace("/", "\\")
            readers = {}
            for bits in (32, 64):
                exe = ROOT / f"target/winmap-probe/winmap-probe{bits}.exe"
                if not exe.is_file():
                    raise RuntimeError(f"build missing {exe}")
                log = (artifacts / f"wine-probe{bits}.log").open("w")
                err = (artifacts / f"wine-probe{bits}-stderr.log").open("w")
                handles.extend((log, err))
                child = subprocess.Popen([str(wine), str(exe), "--path", dos_path,
                                          "--duration-secs", "10"],
                                         env=env, stdin=subprocess.DEVNULL,
                                         stdout=subprocess.PIPE, stderr=err, text=True)
                processes.append(child)
                readers[bits] = child
                thread = threading.Thread(target=collect, args=(bits, child, log), daemon=True)
                thread.start()
                threads.append(thread)

            wait_both(lambda event: event["state"] == "unavailable")
            producer = start_producer(path, 0)
            first_live = wait_both(lambda event: event["state"] == "live")
            inode_before = path.stat().st_ino
            killed_at = time.monotonic()
            producer.kill()
            assert producer.wait(timeout=3) == -9
            stale = wait_both(lambda event: event["state"] == "stale")
            stale_delay_ms = {bits: event["at_ms"] - round((killed_at - started) * 1000)
                              for bits, event in stale.items()}
            assert all(0 <= value <= 1500 for value in stale_delay_ms.values()), stale_delay_ms

            producer = start_producer(path, 2)
            resumed = wait_both(lambda event: event["state"] == "live"
                                and event.get("session") != first_live[event["bits"]]["session"])
            inode_after = path.stat().st_ino
            assert inode_before == inode_after, "producer replaced the mapped inode"
            assert producer.wait(timeout=4) == 0
            wait_both(lambda event: event["state"] == "offline"
                      and event.get("session") == resumed[event["bits"]]["session"])
            for child in readers.values():
                assert child.wait(timeout=12) == 0, "PE reader failed; inspect artifacts logs"
            for thread in threads:
                thread.join(timeout=2)
            version = subprocess.check_output([str(wine), "--version"], env=env, text=True).strip()
            report = {
                "result": "PASS",
                "wine": str(wine),
                "wine_version": version,
                "isolated_prefix": str(prefix),
                "reader_bits": sorted(readers),
                "native_path": str(path),
                "windows_path": dos_path,
                "same_inode_across_restart": inode_before == inode_after,
                "sigkill_to_stale_zero_ms": stale_delay_ms,
                "reader_returncodes": {bits: child.returncode for bits, child in readers.items()},
                "checks": ["reader before producer", "real Win32 shared file mapping",
                           "synthetic verification of all pressure bytes", "SIGKILL to safe zero",
                           "same mapping/inode survives producer restart", "new session returns live",
                           "graceful shutdown publishes offline zero"],
                "observations": observations,
                "notes": ["Synthetic data only; not a HID-to-Wine or game integration test.",
                          "Timing is local observation, not game input latency.",
                          "Temporary /dev/shm input is removed only after readers exit."],
            }
            (artifacts / "wine-validation.json").write_text(
                json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
            print(json.dumps({key: value for key, value in report.items()
                              if key != "observations"}, ensure_ascii=False, indent=2))
            print("Full evidence: artifacts/wine-validation.json and wine-probe{32,64}.log")
    finally:
        stop_children()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wine", type=pathlib.Path, required=True)
    args = parser.parse_args()
    validate(args.wine.resolve())


if __name__ == "__main__":
    main()
