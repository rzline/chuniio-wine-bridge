#!/usr/bin/env python3
"""Measure synthetic mmap -> actual DLL callback -> host stdout observation.

No HID access, game launch, game-prefix access, or production mapping writes.
This is a software-path observation including pipe/collector overhead, NOT
USB-to-callback, touch-to-display latency, or a hard worst-case guarantee.
"""
import argparse
import ctypes
import fcntl
import hashlib
import json
import mmap
import os
import pathlib
import random
import statistics
import subprocess
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]


def summarize(samples):
    if not samples:
        raise RuntimeError("no measured callback events")
    ordered = sorted(samples)
    percentile = lambda fraction: ordered[max(0, int((len(ordered) - 1) * fraction))]
    return {"samples": len(samples), "mean_ms": round(statistics.mean(samples), 3),
            "p50_ms": round(percentile(.5), 3), "p95_ms": round(percentile(.95), 3),
            "p99_ms": round(percentile(.99), 3), "max_ms": round(ordered[-1], 3)}


def measure(wine, dll, arch, ini_path=None):
    prefix = ROOT / "artifacts/wine-prefix"
    if not (prefix / "system.reg").is_file():
        raise RuntimeError("initialize the isolated artifacts/wine-prefix first")
    run = pathlib.Path(tempfile.mkdtemp(prefix="chuniio-latency-", dir=ROOT / "artifacts"))
    report = {"result": "FAIL", "scope": __doc__.strip(), "dll": str(dll),
              "dll_sha256": hashlib.sha256(dll.read_bytes()).hexdigest(), "arch": arch,
              "directory": str(run), "writer_interval_ms": [4, 7], "warmup_secs": 1}
    atomic = ctypes.CDLL("libatomic.so.1").__atomic_store_4
    atomic.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int]
    atomic.restype = None
    published, received, summaries = {}, [], []
    ready = threading.Event()
    child, collector = None, None
    try:
        with tempfile.TemporaryDirectory(prefix="chuni-latency-", dir="/dev/shm") as directory:
            path = pathlib.Path(directory) / "input"
            fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_EXCL, 0o600)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                os.ftruncate(fd, 4096)
                with mmap.mmap(fd, 4096) as region:
                    address = ctypes.addressof(ctypes.c_char.from_buffer(region))
                    store = lambda word, value: atomic(address + word * 4, value & 0xffffffff, 5)
                    store(1, 2)
                    store(2, 4096)
                    store(17, random.randrange(1, 2**32))
                    store(18, 1)
                    store(20, 1)  # Explicitly synthetic, never HID.
                    store(0, 0x43504231)
                    ini = ini_path or run / "segatools.ini"
                    if ini_path is None:
                        ini.write_text("[io3]\ntest=0\nservice=0\ncoin=0\n")
                    report["ini"] = str(ini)
                    report["ini_sha256"] = hashlib.sha256(ini.read_bytes()).hexdigest()
                    report["keyboard_polling"] = "supplied INI" if ini_path else "disabled"
                    dos = lambda p: "Z:" + str(p).replace("/", "\\")
                    env = dict(os.environ, WINEPREFIX=str(prefix), WINEDEBUG="-all",
                               WINEDLLOVERRIDES="mscoree,mshtml=",
                               CHUNI_BRIDGE_PATH=dos(path), CHUNI_BRIDGE_INI=dos(ini))
                    with (run / "probe.log").open("w") as log, (run / "stderr.log").open("w") as err:
                        def collect():
                            for line in child.stdout:
                                timestamp = time.monotonic_ns()
                                log.write(line)
                                if line.startswith("latency_id="):
                                    received.append((int(line.split("=", 1)[1]), timestamp))
                                elif line.startswith("dll_loaded "):
                                    ready.set()
                                elif line.startswith("summary "):
                                    summaries.append(line.strip())
                            log.flush()

                        exe = ROOT / "target/chuniio" / arch / "chuniio-probe.exe"
                        child = subprocess.Popen([str(wine), str(exe), "--dll", dos(dll),
                                                  "--duration-secs", "6", "--latency-events"],
                                                 env=env, stdin=subprocess.DEVNULL,
                                                 stdout=subprocess.PIPE, stderr=err, text=True)
                        collector = threading.Thread(target=collect, daemon=True)
                        collector.start()
                        try:
                            if not ready.wait(8):
                                raise RuntimeError("DLL consumer did not become ready")
                            start = time.monotonic_ns()
                            identifier = 0
                            while time.monotonic_ns() - start < 4_000_000_000:
                                if child.poll() is not None:
                                    raise RuntimeError("DLL consumer exited during measurement")
                                identifier += 1
                                store(16, 2 * identifier - 1)
                                store(18, identifier + 1)
                                store(19, 1)
                                store(24, identifier)
                                store(25, identifier ^ 0xffffffff)
                                # Timestamp immediately before the release of this snapshot.
                                published[identifier] = time.monotonic_ns()
                                store(16, 2 * identifier)
                                time.sleep(random.uniform(.004, .007))
                            store(16, 2 * identifier + 1)
                            store(18, identifier + 2)
                            store(19, 0)
                            store(24, 0)
                            store(25, 0)
                            store(16, 2 * identifier + 2)
                            if child.wait(timeout=8) != 0:
                                raise RuntimeError("DLL consumer failed; inspect probe/stderr logs")
                            collector.join(timeout=3)
                            if collector.is_alive():
                                raise RuntimeError("output collector did not stop")
                        finally:
                            if child.poll() is None:
                                child.kill()
                            child.wait(timeout=3)
                            collector.join(timeout=3)
                            child.stdout.close()
                    if len(summaries) != 1 or "bad_callbacks=0" not in summaries[0]:
                        raise RuntimeError("missing or invalid callback summary")
                    samples, measured_ids = [], set()
                    for identifier, observed in received:
                        if identifier not in published:
                            raise RuntimeError("callback returned an unknown synthetic ID")
                        sent = published[identifier]
                        if sent < start + 1_000_000_000:
                            continue
                        if observed < sent:
                            raise RuntimeError("negative observation interval")
                        samples.append((observed - sent) / 1_000_000)
                        measured_ids.add(identifier)
                    expected = sum(t >= start + 1_000_000_000 for t in published.values())
                    if len(measured_ids) < 100:
                        raise RuntimeError("insufficient callback events")
                    report.update(result="PASS", measurements=summarize(samples),
                                  published_after_warmup=expected,
                                  unobserved_ids_after_warmup=expected - len(measured_ids),
                                  summary=summaries[0], reader_returncode=child.returncode)
            finally:
                os.close(fd)
        report["temporary_mapping_removed"] = not path.exists()
    except BaseException as error:
        report["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        (run / "result.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wine", type=pathlib.Path, required=True)
    parser.add_argument("--arch", choices=("x86", "x64"), default="x86")
    parser.add_argument("--dll", type=pathlib.Path)
    parser.add_argument("--ini", type=pathlib.Path,
                        help="read an existing INI without changing it; the test mapping still overrides shmPath")
    args = parser.parse_args()
    dll = args.dll or ROOT / "target/chuniio" / args.arch / "chuniio.dll"
    measure(args.wine.resolve(), dll.resolve(), args.arch,
            args.ini.resolve() if args.ini else None)


if __name__ == "__main__":
    main()
