#!/usr/bin/env python3
"""Verify real HID -> experimental SHM -> both Win32 PE readers.

No game, HID writes, or default/game prefix access. Default mode is passive:
it does NOT require the operator to touch/unplug anything. --require-activity
and --reconnect are only for separately announced, operator-confirmed captures.
The tool never disconnects hardware, modifies permissions, or restarts readers.
"""

import argparse
import contextlib
import json
import os
import pathlib
import queue
import re
import signal
import subprocess
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
STATES = {"waiting", "live", "offline", "stale", "busy", "unavailable"}


def parse_frame(line):
    """Parse input-only trace/state lines; fail closed on malformed payloads."""
    if line.startswith("sample "):
        frame = {"state": "sample"}
        required = {"session", "heartbeat", "online", "source", "air", "buttons"}
    elif line.startswith("state="):
        state = line.split()[0].split("=", 1)[1]
        if state not in STATES:
            raise ValueError(f"unknown state: {state}")
        frame = {"state": state}
        required = set() if state == "unavailable" else {"air", "buttons"}
        if state in {"waiting", "live", "offline", "stale"}:
            required |= {"session", "heartbeat", "source"}
    else:
        return None
    for key, value in re.findall(r"\b(session|heartbeat|online|source|air|buttons)=(\d+)\b", line):
        if key in frame:
            raise ValueError(f"duplicate field: {key}")
        frame[key] = int(value)
    if not required.issubset(frame):
        raise ValueError(f"missing required fields: {required - frame.keys()}")
    for key in ("session", "heartbeat", "source"):
        if key in frame and not 0 <= frame[key] <= 0xFFFFFFFF:
            raise ValueError(f"out-of-range {key}")
    for key in ("air", "buttons"):
        if key in frame and not 0 <= frame[key] <= 255:
            raise ValueError(f"out-of-range {key}")
    if "online" in frame and frame["online"] not in (0, 1):
        raise ValueError("online must be 0/1")
    match = re.search(r"\bslider=(\[.*\])$", line)
    if not match:
        raise ValueError("missing slider")
    slider = [0] * 32 if match[1] == "[0;32]" else json.loads(match[1])
    if (not isinstance(slider, list) or len(slider) != 32
            or any(type(value) is not int or not 0 <= value <= 255 for value in slider)):
        raise ValueError("expected exactly 32 pressure bytes")
    frame["slider"] = slider
    safe_zero = frame["state"] not in {"live", "sample"} or frame.get("online") == 0
    if safe_zero and (any(slider) or frame.get("air", 0) or frame.get("buttons", 0)):
        raise ValueError("non-live/offline input must be zero")
    return frame


def verify_frames(trace, observations, require_activity=False, require_idle=False):
    """Correlate each live PE sample by session+heartbeat with the native trace."""
    index = {}
    for frame in trace:
        assert frame["state"] == "sample" and frame["source"] == 2, frame
        key = (frame["session"], frame["heartbeat"])
        assert key not in index, f"duplicate native publication: {key}"
        index[key] = frame
    assert index, "no native HID trace"
    assert trace[-1]["online"] == 0, "native producer did not shut down offline"
    live = {32: 0, 64: 0}
    active = {32: 0, 64: 0}
    channels = {32: set(), 64: set()}
    for frame in observations:
        if frame["state"] != "live":
            continue
        bits = frame["bits"]
        assert frame["source"] == 2, "reader did not consume source=HID"
        key = (frame["session"], frame["heartbeat"])
        assert key in index, f"PE{bits}: publication missing from trace: {key}"
        native = index[key]
        assert native["online"] == 1, f"PE{bits}: offline publication became live"
        for field in ("air", "buttons", "slider"):
            assert frame[field] == native[field], f"PE{bits}: mismatched {field} at {key}"
        live[bits] += 1
        if any(frame["slider"]):
            active[bits] += 1
            channels[bits].update(i for i, value in enumerate(frame["slider"]) if value)
        if require_idle:
            assert not any(frame["slider"]) and frame["air"] == frame["buttons"] == 0, \
                f"PE{bits}: operator-confirmed idle capture contained activity"
    assert all(live.values()), f"both PE readers must see live HID samples: {live}"
    if require_activity:
        assert all(active.values()), f"both PE readers must see nonzero pressure: {active}"
    return {"matched_live_samples": live, "pressure_active_live_samples": active,
            "observed_raw_channels": {bits: sorted(values) for bits, values in channels.items()},
            "native_trace_samples": len(trace)}


def verify_reconnect_frames(trace, observations, minimum_offline_ms=1000):
    """Require one held-input loss/recovery cycle BEFORE normal shutdown.

    observations must be the saved prefix processed before sending SIGTERM;
    shutdown offline frames must never qualify as a hardware disconnect.
    All comparisons use trace positions, not numerical heartbeat ordering.
    """
    verify_frames(trace, observations, require_activity=True)
    sessions = {f["session"] for f in trace}
    assert len(sessions) == 1, "native producer session changed during reconnect"
    assert len(trace) >= 2 and trace[-2]["online"] == 1, "native was not online immediately before shutdown"
    index = {(f["session"], f["heartbeat"]): i for i, f in enumerate(trace)}
    gaps, pending = [], None
    for i in range(1, len(trace)):
        if trace[i - 1]["online"] and not trace[i]["online"]:
            pending = i
        elif pending is not None and trace[i]["online"]:
            gaps.append((pending, i))
            pending = None
    assert len(gaps) == 1, f"expected one completed native offline/recovery gap; got {len(gaps)}"
    start, resume = gaps[0]
    assert any(trace[start - 1]["slider"]), "last native report before disconnect was not pressed"
    assert not any(trace[resume]["slider"]), "first resumed native report was not released"

    def position(frame):
        assert frame["source"] == 2, "non-HID publication in reconnect cycle"
        key = (frame["session"], frame["heartbeat"])
        assert key in index, f"reconnect sample missing native publication: {key}"
        return index[key]

    def zero(frame):
        return not any(frame["slider"]) and frame["air"] == frame["buttons"] == 0

    readers = {}
    for bits in (32, 64):
        frames = [f for f in observations if f["bits"] == bits]
        assert frames and frames[-1]["state"] == "live", f"PE{bits}: capture did not end healthy/live"
        live = [f for f in frames if f["state"] == "live"]
        before = [f for f in live if position(f) < start]
        assert before and any(before[-1]["slider"]), f"PE{bits}: last pre-disconnect live sample not pressed"
        offline = [f for f in frames if f["state"] == "offline" and start <= position(f) < resume]
        assert len(offline) >= 2, f"PE{bits}: missing repeated offline samples before shutdown"
        assert all(zero(f) and not trace[position(f)]["online"] for f in offline), \
            f"PE{bits}: unsafe or inconsistent offline publication"
        heartbeat_count = len({f["heartbeat"] for f in offline})
        assert heartbeat_count >= 2, f"PE{bits}: offline process heartbeats did not advance"
        span = offline[-1]["at_ms"] - offline[0]["at_ms"]
        assert span >= minimum_offline_ms, f"PE{bits}: offline observation too short: {span} ms"
        after = [f for f in live if position(f) >= resume]
        assert after and not any(after[0]["slider"]), f"PE{bits}: no released live sample on recovery"
        active = [i for i, f in enumerate(after) if any(f["slider"])]
        assert active, f"PE{bits}: no new pressure after recovery"
        assert any(not any(f["slider"]) for f in after[active[0] + 1:]), \
            f"PE{bits}: recovered pressure did not release while live"
        tail = 0
        for frame in reversed(after):
            if not zero(frame):
                break
            tail += 1
        assert tail >= 2, f"PE{bits}: final live inputs not stably zero"
        readers[bits] = {"pressed_before_disconnect": True,
                         "offline_zero_samples": len(offline),
                         "distinct_offline_heartbeats": heartbeat_count,
                         "observed_offline_span_ms": span,
                         "first_recovered_live_pressure_zero": True,
                         "recovered_pressure_samples": len(active),
                         "trailing_all_input_zero_live_samples": tail}
    return {"native_completed_gap_count": len(gaps), "native_session": next(iter(sessions)),
            "same_session_across_reconnect": True, "native_pressed_before_disconnect": True,
            "first_native_recovered_pressure_zero": True,
            "reader_reconnect_checks": readers}


def wait_start_gate(directory, payload, receive, alive, healthy, timeout):
    """Publish readiness only for this run; consume START before acknowledging.

    The caller must use a fresh private control directory. This does not grant
    permission for hardware actions: the operator must separately confirm them.
    """
    if timeout <= 0:
        raise ValueError("start gate timeout must be positive")
    if not alive() or not healthy():
        raise RuntimeError("cannot arm without both readers currently live")
    directory.mkdir(mode=0o700)  # Refuse all stale/reused control directories.
    ready = directory / "ready.json"
    try:
        with ready.open("x", encoding="utf-8") as stream:
            json.dump(dict(payload, state="ready", driver_pid=os.getpid()), stream)
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("start gate expired; no hardware action should begin")
            if not alive():
                raise RuntimeError("a process exited while waiting for START")
            frame = receive(min(remaining, 0.1))
            if frame and frame["state"] in {"eof", "offline", "stale", "unavailable", "waiting"}:
                raise RuntimeError(f"link lost before START: {frame}")
            if not alive():
                raise RuntimeError("a process exited while waiting for START")
            if time.monotonic() >= deadline:
                raise TimeoutError("start gate expired; no hardware action should begin")
            if (directory / "start").is_file() and healthy():
                acknowledgement = {"state": "started", "driver_pid": os.getpid()}
                with (directory / "started.json").open("x", encoding="utf-8") as stream:
                    json.dump(acknowledgement, stream)
                return acknowledgement
    finally:
        ready.unlink(missing_ok=True)


def validate(wine, observe_secs, output, require_activity=False, require_idle=False, reconnect=False,
             arm_dir=None, arm_timeout_secs=60):
    if reconnect and (require_activity or require_idle):
        raise ValueError("reconnect is a separate operator-confirmed capture mode")
    if reconnect and arm_dir is None:
        raise ValueError("reconnect requires an arm directory and explicit START acknowledgement")
    if not 1 <= arm_timeout_secs <= 300:
        raise ValueError("arm timeout must be in 1..300 seconds")
    if arm_dir is not None and (arm_dir.exists() or not arm_dir.parent.is_dir()):
        raise ValueError("arm directory must be new, under an existing private parent")
    artifacts = ROOT / "artifacts"
    prefix = artifacts / "wine-prefix"
    if not (prefix / "system.reg").is_file():
        raise RuntimeError("initialize artifacts/wine-prefix with this runner first")
    artifacts.mkdir(exist_ok=True)
    evidence = pathlib.Path(tempfile.mkdtemp(prefix="hid-wine-", dir=artifacts))
    env = dict(os.environ, WINEPREFIX=str(prefix), WINEDEBUG="-all",
               WINEDLLOVERRIDES="mscoree,mshtml=")
    events = queue.Queue()
    processes, handles, threads, observations = [], [], [], []
    latest = {}
    started = time.monotonic()
    native_log = evidence / "producer.log"
    report = {"result": "FAIL", "wine": str(wine), "isolated_prefix": str(prefix),
              "evidence_directory": str(evidence), "observation_secs": observe_secs,
              "require_activity": require_activity, "require_idle": require_idle,
              "reconnect": reconnect, "observations": observations,
              "arm_directory": str(arm_dir) if arm_dir is not None else None}

    def collect(bits, child, log):
        for line in child.stdout:
            line = line.rstrip()
            log.write(line + "\n")
            log.flush()
            events.put((time.monotonic(), bits, line))
        events.put((time.monotonic(), bits, None))

    def receive(timeout):
        try:
            timestamp, bits, line = events.get(timeout=timeout)
        except queue.Empty:
            return None
        if line is None:
            return {"state": "eof", "bits": bits}
        if line.startswith("error="):
            raise AssertionError(f"PE{bits}: {line}")
        frame = parse_frame(line)
        if frame is not None:
            frame.update(bits=bits, at_ms=round((timestamp - started) * 1000))
            observations.append(frame)
            latest[bits] = frame
        return frame

    def wait_both(state, timeout):
        matched = set()
        deadline = time.monotonic() + timeout
        while len(matched) != 2:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(f"timeout waiting for both readers state={state}; got={matched}")
            frame = receive(min(remaining, 0.2))
            if frame and frame["state"] == "eof":
                raise AssertionError(f"PE{frame['bits']} exited before state={state}")
            if frame and frame["state"] == state:
                matched.add(frame["bits"])

    def stop_children():
        for child in processes:
            if child.poll() is None:
                with contextlib.suppress(ProcessLookupError):
                    child.terminate()
                try:
                    child.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=3)
        for thread in threads:
            thread.join(timeout=2)
        for handle in handles:
            handle.close()

    try:
        with tempfile.TemporaryDirectory(prefix="chuni-hid-wine-", dir="/dev/shm") as directory, contextlib.ExitStack() as cleanup:
            cleanup.callback(stop_children)  # Before TemporaryDirectory removes mappings.
            path = pathlib.Path(directory) / "input"
            dos_path = "Z:" + str(path).replace("/", "\\")
            report.update(native_path=str(path), windows_path=dos_path)
            log = native_log.open("w", encoding="utf-8")
            handles.append(log)
            native_command = [str(ROOT / "target/release/chuni-bridge-probe"),
                              "bridge", "--path", str(path), "--trace"]
            if arm_dir is not None:
                # Bound orphan lifetimes even if the supervising Python is SIGKILLed.
                native_command += ["--duration-secs", str(observe_secs + arm_timeout_secs + 90)]
            producer = subprocess.Popen(native_command, stdin=subprocess.DEVNULL,
                                        stdout=log, stderr=subprocess.STDOUT)
            processes.append(producer)
            report["native_pid"] = producer.pid
            # Hardware/permissions must work before starting a timed PE reader.
            with native_log.open(encoding="utf-8") as stream:
                deadline = time.monotonic() + 8
                while True:
                    line = stream.readline()
                    frame = parse_frame(line.rstrip()) if line else None
                    if frame and frame["state"] == "sample" and frame["online"]:
                        break
                    if producer.poll() is not None or time.monotonic() >= deadline:
                        raise AssertionError(f"no valid HID report; inspect {native_log}")
                    if not line:
                        time.sleep(0.01)
            readers = {}
            for bits in (32, 64):
                log = (evidence / f"probe{bits}.log").open("w", encoding="utf-8")
                err = (evidence / f"probe{bits}-stderr.log").open("w", encoding="utf-8")
                handles.extend((log, err))
                exe = ROOT / f"target/winmap-probe/winmap-probe{bits}.exe"
                reader_command = [str(wine), str(exe), "--path", dos_path, "--observe",
                                  "--duration-secs", str(observe_secs + 6 if arm_dir is None
                                                         else observe_secs + arm_timeout_secs + 46)]
                if arm_dir is not None:
                    reader_command += ["--stop-path", "Z:" + str(arm_dir / "stop").replace("/", "\\")]
                child = subprocess.Popen(reader_command, env=env, stdin=subprocess.DEVNULL,
                                         stdout=subprocess.PIPE, stderr=err, text=True)
                processes.append(child)
                readers[bits] = child
                report.setdefault("reader_pids", {})[bits] = child.pid
                thread = threading.Thread(target=collect, args=(bits, child, log), daemon=True)
                threads.append(thread)
                thread.start()
            wait_both("live", timeout=40)
            initial_stat = path.stat()
            report["initial_inode"] = [initial_stat.st_dev, initial_stat.st_ino]
            if arm_dir is not None:
                report["armed_at_ms"] = round((time.monotonic() - started) * 1000)
                payload = {key: report[key] for key in ("native_pid", "reader_pids", "native_path")}
                wait_start_gate(
                    arm_dir, payload, receive,
                    alive=lambda: all(child.poll() is None for child in processes),
                    healthy=lambda: all(latest.get(bits, {}).get("state") == "live"
                                        and latest[bits]["source"] == 2 for bits in (32, 64)),
                    timeout=arm_timeout_secs)
                report["started_at_ms"] = round((time.monotonic() - started) * 1000)
            deadline = time.monotonic() + observe_secs
            while time.monotonic() < deadline:
                if producer.poll() is not None:
                    raise AssertionError("native bridge exited during observation")
                frame = receive(min(0.2, max(0, deadline - time.monotonic())))
                forbidden = {"eof", "unavailable"} if reconnect else {"eof", "offline", "stale", "unavailable"}
                if frame and frame["state"] in forbidden:
                    raise AssertionError(f"link lost during observation: {frame}")
            # Freeze a prefix BEFORE requesting stop. Otherwise normal shutdown
            # could be incorrectly counted as a hardware disconnect.
            report["observation_frame_count"] = len(observations)
            report["observation_ended_at_ms"] = round((time.monotonic() - started) * 1000)
            assert all(child.poll() is None for child in readers.values()), "reader exited during capture"
            producer.send_signal(signal.SIGTERM)
            assert producer.wait(timeout=3) == 0, "native bridge failed graceful shutdown"
            wait_both("offline", timeout=3)
            if arm_dir is not None:
                (arm_dir / "stop").touch(exist_ok=False)
            for bits, child in readers.items():
                assert child.wait(timeout=3 if arm_dir is not None else observe_secs + 12) == 0, f"PE{bits} failed"
            for thread in threads:
                thread.join(timeout=2)
                assert not thread.is_alive(), "reader log collector did not finish"
            while not events.empty():
                receive(0)
            final_stat = path.stat()
            report["same_inode_before_after"] = (initial_stat.st_dev, initial_stat.st_ino) == \
                (final_stat.st_dev, final_stat.st_ino)
            assert report["same_inode_before_after"], "backing inode changed"
            native_text = native_log.read_text(encoding="utf-8")
            trace = []
            for line in native_text.splitlines():
                frame = parse_frame(line)
                if frame and frame["state"] == "sample":
                    trace.append(frame)
            metrics = verify_frames(trace, observations, require_activity, require_idle)
            report["checks"] = ["real HID source=2", "both PE readers heartbeat-confirmed live",
                                "each live PE sample matches native session/heartbeat and all input bytes",
                                "normal stop publishes offline zero to both PE readers"]
            if reconnect:
                paths = re.findall(r"^bridge state=connected path=(\S+) waiting_for_valid_report$",
                                   native_text, re.MULTILINE)
                report["connected_device_paths"] = paths
                report["device_node_path_changed"] = len(set(paths)) > 1
                report["reader_mapping_counts"] = {
                    bits: (evidence / f"probe{bits}.log").read_text().splitlines().count("mapped_bytes=4096")
                    for bits in (32, 64)}
                assert all(n == 1 for n in report["reader_mapping_counts"].values()), "reader mapping reopened"
                report["native_permission_denied_observed"] = "Permission denied" in native_text
                assert len(paths) >= 2, ("native log did not confirm two HID opens; "
                                         f"permission_denied={report['native_permission_denied_observed']}")
                report["reconnect_checks"] = verify_reconnect_frames(
                    trace, observations[:report["observation_frame_count"]])
                report["checks"] += ["held input -> offline zero with advancing heartbeats -> released live",
                                     "new post-recovery press and live release for both readers",
                                     "same native session, same inode, one reader process/mapping per architecture"]
            report.update(result="PASS", reader_bits=[32, 64],
                          wine_version=subprocess.check_output([str(wine), "--version"],
                                                               env=env, text=True).strip(),
                          reader_returncodes={bits: child.returncode for bits, child in readers.items()},
                          **metrics)
            report["notes"] = ["Experimental bridge; no chuniio DLL or game integration.",
                               "Passive mode alone does not require or prove nonzero Wine pressure.",
                               "Physical unplug/replug must be performed by the confirmed operator; never by this tool."
                               if reconnect else "No physical unplug/replug is performed by this tool.",
                               "Node renumbering is only observed when device_node_path_changed is true.",
                               "Trace is input-only; no card bytes. Timing is not game input latency."]
    except BaseException as error:
        report.update(error=f"{type(error).__name__}: {error}")
        raise
    finally:
        stop_children()
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({key: value for key, value in report.items() if key != "observations"},
                         ensure_ascii=False, indent=2), flush=True)
        print(f"Evidence: {output}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wine", type=pathlib.Path, required=True)
    parser.add_argument("--observe-secs", type=int, default=5)
    parser.add_argument("--arm-dir", type=pathlib.Path,
                        help="fresh private control dir: wait for start, acknowledge started.json; no operator actions yet")
    parser.add_argument("--arm-timeout-secs", type=int, default=60)
    parser.add_argument("--output", type=pathlib.Path,
                        help="JSON result; defaults to a separate reconnect report in reconnect mode")
    expectation = parser.add_mutually_exclusive_group()
    expectation.add_argument("--require-activity", action="store_true")
    expectation.add_argument("--require-idle", action="store_true")
    expectation.add_argument("--reconnect", action="store_true",
                             help="operator-confirmed held-input unplug/replug capture; no automatic hardware actions")
    args = parser.parse_args()
    if not __debug__:
        parser.error("do not use -O or PYTHONOPTIMIZE: evidence checks require assertions")
    if not 1 <= args.arm_timeout_secs <= 300:
        parser.error("--arm-timeout-secs must be in 1..300")
    if not 1 <= args.observe_secs <= 120:
        parser.error("--observe-secs must be in 1..120")
    if args.reconnect and args.observe_secs < 20:
        parser.error("--reconnect requires --observe-secs in 20..120")
    if args.reconnect and args.arm_dir is None:
        parser.error("--reconnect requires --arm-dir and explicit START acknowledgement")
    output = args.output or ROOT / "artifacts" / (
        "hid-wine-reconnect-validation.json" if args.reconnect else "hid-wine-validation.json")
    def interrupted(signum, _frame):
        raise KeyboardInterrupt(f"signal {signum}; stopping owned probe processes")

    signal.signal(signal.SIGTERM, interrupted)
    validate(args.wine.resolve(), args.observe_secs, output.resolve(),
             args.require_activity, args.require_idle, args.reconnect,
             args.arm_dir.resolve() if args.arm_dir is not None else None, args.arm_timeout_secs)


if __name__ == "__main__":
    main()
