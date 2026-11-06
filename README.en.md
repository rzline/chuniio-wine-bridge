# chuniio-wine-bridge

**English** | [简体中文](README.md)

A low-latency RPunithm controller bridge for **Linux + Wine**, transferring input and slider LED data between native Linux `hidraw` access and the Wine-side `chuniio` interface via a file-backed shared memory region.

The project consists of the Rust CLI `chuni-bridge-probe`, the Windows `chuniio.dll` (32-bit and 64-bit), and companion diagnostic utilities. Native Linux handles all hardware HID I/O; the Wine DLL only reads and writes shared memory and never touches USB/HID directly.

## Table of Contents

- [Features & Architecture](#features--architecture)
- [Requirements](#requirements)
- [Quick Start](#quick-start)
- [Building & Configuring the Wine DLL](#building--configuring-the-wine-dll)
- [Command Reference](#command-reference)
- [LED Debugging](#led-debugging)
- [Shared Memory & Fail-Safes](#shared-memory--fail-safes)
- [Testing & Validation](#testing--validation)
- [Troubleshooting](#troubleshooting)
- [Project Layout](#project-layout)
- [Contributing](#contributing)

## Features & Architecture

- **Native HID Input**: Matches RPunithm `VID:PID = 1973:2001`, interface `2` by default. Strictly parses 45-byte input reports to extract 32 pressure channels, air sensor bits, and cabinet buttons.
- **Cross-Process Input Delivery**: The Linux producer publishes lock-free input snapshots in raw hardware channel order; the Wine DLL permutes channels into game order at the `chuniio` API boundary.
- **Slider LED Feedback**: The Wine DLL publishes game LED payloads into shared memory, and the Linux `bridge` assembles them into HID output reports to drive the 31 physical slider LEDs on verified hardware.
- **Fail-Safe Zeroing & Rediscovery**: Zeros hardware input on read errors, report timeouts, or producer staleness, while polling on the Linux side to rediscover the controller across reconnects.
- **Diagnostics & Validation Suite**: Includes utilities for device enumeration, report capture, synthetic input generation, hardware LED calibration, dual-bitness (PE32/PE32+) Wine validation, and software-path latency measurement.

By default, the shared backing file resides at `/dev/shm/chuni_bridge_probe` on Linux and is accessed via `Z:\dev\shm\chuni_bridge_probe` inside Wine. Both sides memory-map the **exact same backing file**, rather than relying on isolated named shared-memory namespaces.

### Support Matrix

| Component | Current Scope |
| --- | --- |
| Native Bridge | x86 / x86_64 Linux, HIDAPI `hidraw` backend |
| Wine Interface | x86 / x64 Windows PE DLL, `chuniio` API `0x0102` |
| Target Hardware | RPunithm `1973:2001`, fixed to USB interface `2` |
| Verified Runners | Wine 10.20; other Wine / Proton builds require separate verification |
| Slider LEDs | 32 game zones mapped to 31 physical slider LEDs; zone `31` has no physical LED |
| Air Tower LEDs & Card Reader | Verified ToF controller has no air tower LEDs (air LED color words are retained in shared memory for compatibility without hardware output); card reader is not implemented |

## Requirements

### Linux CLI

- Rust **1.89 or newer** and Cargo.
- A C toolchain and `pkg-config`.
- System HIDAPI development files providing `hidapi-hidraw.pc`.
- Read/write permissions for the current user on the target interface's `/dev/hidraw*` node.

Verify dependencies:

```sh
rustc --version
cargo --version
pkg-config --modversion hidapi-hidraw
```

### Optional Development Tools

- **DLL / PE Probe Build**: Clang, LLD (`lld-link`), `llvm-nm`, `llvm-dlltool`, Python 3.9+, and a Wine installation tree containing Windows development headers (`include/wine/windows` and `include/wine/msvcrt`).
- **Wine Validation**: A Wine runner capable of executing both 32-bit and 64-bit PE binaries; building `winmap-probe` also requires matching Wine import libraries (`libkernel32.a`, `libshell32.a`).
- **Python Diagnostics**: Python 3.9+; the latency measurement script additionally requires `libatomic.so.1`.

## Quick Start

Run all commands from the repository root. **Do not run the bridge or the game as `root`.**

### 1. Build (or Download)

> Prebuilt binaries (`chuni-bridge-probe`, x86/x64 `chuniio.dll`, and udev rules) are also available on the [GitHub Releases](../../releases) page.

```sh
cargo build --locked --release
./target/release/chuni-bridge-probe --help
```

### 2. Inspect Hardware

Connect the controller and run:

```sh
./target/release/chuni-bridge-probe hid-list
./target/release/chuni-bridge-probe hid-read --reports 20 --duration-secs 10
```

Use `hid-list --all` to list every HID interface on the system. Device selection deduplicates by path and refuses to guess when multiple matching controllers are attached; you can pass the enumerated node explicitly:

```sh
./target/release/chuni-bridge-probe hid-read --device-path /dev/hidrawN
```

Replace `/dev/hidrawN` with the actual node from `hid-list`. Because `hidraw` numbers can change across replugs, do not hardcode it into permanent configurations.

### 3. Configure Device Permissions

The repository includes a `udev` rule scoped strictly to interface `2` of the target controller:

```sh
sudo install -Dm644 packaging/udev/70-rpunithm-slider.rules \
  /etc/udev/rules.d/70-rpunithm-slider.rules
sudo udevadm control --reload-rules
```

Replug the controller so the active local session receives the `uaccess` ACL. When running over SSH or in a headless/seatless session, configure group permissions appropriately instead of making the device world-writable (`0666`).

For temporary diagnostics on the **currently enumerated node**:

```sh
# Replace /dev/hidrawN with the actual node for interface 2; resets on replug.
sudo setfacl -m "u:$(id -un):rw" /dev/hidrawN
```

### 4. Start the Live Bridge

Terminal A:

```sh
./target/release/chuni-bridge-probe bridge
```

Terminal B:

```sh
./target/release/chuni-bridge-probe read --require-live --duration-secs 10
```

Once the reader confirms an advancing heartbeat, it transitions to `state=live` with `source=2` for real HID input. Touching the slider should produce non-zero pressure values that return to zero on release. Note that `--require-live` only verifies that a valid online snapshot was observed—it does not validate touch sensitivity, channel direction, or full sensor coverage.

`bridge` enables LED forwarding by default; pass `bridge --no-leds` when testing input only. If the device is absent or inaccessible, the bridge stays offline and retries periodically (a running process does not imply the hardware is online). Pressing `Ctrl+C` publishes an offline, all-zero hardware snapshot while preserving the shared file.

## Building & Configuring the Wine DLL

### Building the DLL

Set `WINE_ROOT` to a Wine installation directory that includes `include/wine/windows` and `include/wine/msvcrt`:

```sh
WINE_ROOT="/absolute/path/to/wine"
sh tools/build_chuniio.sh "$WINE_ROOT"
```

Build artifacts:

```text
target/chuniio/x86/chuniio.dll
target/chuniio/x86/chuniio-probe.exe
target/chuniio/x64/chuniio.dll
target/chuniio/x64/chuniio-probe.exe
```

The build script uses Clang/LLVM (`lld-link`, `llvm-nm`, `llvm-dlltool`) to generate COFF import libraries and freestanding PE binaries directly—no MinGW toolchain or CRT is required.

### Wiring into the Game Loader

1. Stop the game and back up your existing DLL and configuration files.
2. Pick the `x86` or `x64` DLL matching the **bitness of the game process loading the DLL** (not the host Linux architecture).
3. Place `chuniio.dll` where your loader can access it and point your `segatools` / loader configuration to it.
4. Ensure the Wine prefix can access the Linux shared file, then start the native `bridge` and launch the game.

Loader-specific DLL path settings vary. The snippet below shows **only the settings read by this project's `chuniio.dll` itself** (`segatools.ini` in the process working directory by default):

```ini
[chuniio]
shmPath=Z:\dev\shm\chuni_bridge_probe

[io3]
test=0x31
service=0x32
coin=0x33
```

The virtual-key codes above bind keys `1`, `2`, and `3` to Test, Service, and Coin respectively; set any value to `0` to disable that keybind.

| Option | Purpose |
| --- | --- |
| `CHUNI_BRIDGE_INI` | Override the INI file path read by the DLL (Wine-visible path) |
| `[chuniio] shmPath` | Wine path to the shared backing file |
| `CHUNI_BRIDGE_PATH` | Override the shared file path (takes precedence over `shmPath` in INI) |
| Native `--path` | Linux path to the shared file; must resolve to the exact same file as the DLL |

The DLL maps the backing file **read-write** (reading input snapshots and writing LED payloads). Even if you only use input, the Wine process must have read/write permissions on the shared file. Adjust drive mappings if your prefix does not expose `/` as `Z:`.

## Command Reference

Pass `--help` to any subcommand for full option details.

| Subcommand | Purpose | Direct HID Access? |
| --- | --- | --- |
| `hid-list` | Enumerate matching controllers (`--all` shows all HID devices) | Enumerates only |
| `hid-read` | Read reports and display both raw and game-order channels | Read-only |
| `bridge` | Run live HID input bridge with optional LED forwarding (on by default) | Reads input; writes LED output |
| `produce` | Publish verifiable synthetic input snapshots without hardware | No |
| `read` | Inspect shared-memory input snapshots and optionally the LED region | No |
| `hid-led` | Send LED calibration patterns directly to hardware | Writes LED output |
| `led-emit` | Publish synthetic LED patterns to shared memory | No (forwarded if `bridge` is running) |

Common `bridge` flags:

| Flag | Default | Description |
| --- | --- | --- |
| `--path` | `/dev/shm/chuni_bridge_probe` | Shared memory backing file path |
| `--retry-ms` | `500` | Re-enumeration interval when device is missing or read fails |
| `--report-timeout-ms` | `100` | Offline threshold when no valid 45-byte HID report arrives |
| `--duration-secs` | `0` | Run duration in seconds (`0` = run indefinitely) |
| `--device-path` | Unset | Pin a specific `/dev/hidrawN` path for diagnostics or multi-device setups |
| `--no-leds` | Disabled | Disable LED forwarding from shared memory to hardware |
| `--trace` | Disabled | Log every input snapshot (verbose; omits card ID bytes) |

Automatic rediscovery works by VID/PID/interface without relying on a fixed `hidrawN` index. Pinning `--device-path` disables path-agnostic recovery if the kernel renumbers the node on replug.

## LED Debugging

### Direct Hardware Calibration

These commands write directly to the controller's LEDs. Stop the game and any running `bridge` first (or run `bridge --no-leds` so two writers do not fight over HID output):

```sh
# Light zone 7 in red for 3 seconds
./target/release/chuni-bridge-probe hid-led --zone 7 --color ff0000 --hold-ms 3000

# Run rainbow animation for 10 seconds
./target/release/chuni-bridge-probe hid-led --rainbow --hold-ms 10000

# Turn all LEDs off
./target/release/chuni-bridge-probe hid-led --off
```

On verified hardware, zone `0` is the rightmost slider key, zone `30` is the leftmost, and zone `31` has no physical LED.

### Inspecting Shared-Memory LEDs

With the game / DLL stopped, run in two terminals:

```sh
# Terminal A: publish synthetic green LEDs to shared memory
./target/release/chuni-bridge-probe led-emit --solid 00ff00

# Terminal B: verify LED publication in shared memory
./target/release/chuni-bridge-probe read --led --require-led --duration-secs 5
```

**Color Ordering Note:** In the live game path, the Wine DLL converts each game `[G, B, R]` triplet into device-ready `[B, R, G]` bytes once, and the Linux bridge forwards them verbatim. By contrast, `hid-led` and `led-emit` take standard `RRGGBB` hex arguments for calibration without passing through the DLL's permutation. Keep a single writer on the LED region at any time.

## Shared Memory & Fail-Safes

The current wire protocol is **v2** over a fixed **4096-byte** page. Both the input and LED regions use `AtomicU32` words with seqlock snapshots, audited specifically for same-machine `x86` / `x86_64` memory ordering.

| Mechanism | Behavior |
| --- | --- |
| Strict Report Validation | Accepts only exact 45-byte input reports; never guesses or strips unexpected Report IDs |
| Report Timeout | Zeros input, closes handle, and retries after 100 ms (default) without a valid report |
| Producer Staleness | Readers zero hardware input if no heartbeat advance is observed within 300 ms |
| Initial Mapping / Session Change | Requires observing a live heartbeat advance before trusting non-zero input from an existing file |
| Torn Snapshot Protection | Bounded seqlock retries; drops to safe state rather than retaining stale presses |
| Graceful Shutdown | Publishes an offline, all-zero snapshot while keeping the file and inode intact for waiting readers |
| Single Input Producer | File lock (`flock`) prevents multiple `bridge` / `produce` instances from writing simultaneously |

**Never delete, replace, or truncate the shared file while any producer or consumer is running**, as doing so splits existing mappings onto an unlinked inode. To reset the file after stopping all processes:

```sh
rm -- /dev/shm/chuni_bridge_probe
```

### Privacy & Security

- Newly created shared files default to mode `0600`; the Linux producer verifies file ownership and refuses to follow symlinks.
- Pressure data is sliced from `report[2..34)`; card reader bytes never enter shared-memory snapshots or `bridge --trace` logs.
- `hid-read --raw` dumps full 45-byte HID reports, which may contain card data—redact before sharing logs.

## Testing & Validation

### Local Checks

```sh
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

### Hardware-Free Synthetic Self-Test

```sh
TEST_DIR="$(mktemp -d)"
./target/release/chuni-bridge-probe produce \
  --path "$TEST_DIR/input" --duration-secs 8 > "$TEST_DIR/producer.log" &
PRODUCER_PID=$!

./target/release/chuni-bridge-probe read \
  --path "$TEST_DIR/input" --verify-synthetic --duration-secs 5

wait "$PRODUCER_PID"
rm -- "$TEST_DIR/input" "$TEST_DIR/producer.log"
rmdir -- "$TEST_DIR"
```

### Dual-Bitness Wine Validation

```sh
WINE_ROOT="/absolute/path/to/wine"
cargo build --locked --release
sh tools/build_winmap_probe.sh "$WINE_ROOT"
sh tools/build_chuniio.sh "$WINE_ROOT"

mkdir -p artifacts
WINEPREFIX="$PWD/artifacts/wine-prefix" WINEARCH=win64 WINEDEBUG=-all \
  WINEDLLOVERRIDES='mscoree,mshtml=' "$WINE_ROOT/bin/wineboot" -i

python3 tools/validate_wine.py --wine "$WINE_ROOT/bin/wine"
python3 tools/validate_chuniio.py --wine "$WINE_ROOT/bin/wine"
```

## Troubleshooting

| Symptom | Resolution |
| --- | --- |
| `hidapi-hidraw` not found | Install your distro's `libhidapi-dev` / `hidapi-devel` package and check `pkg-config --modversion hidapi-hidraw` |
| `Permission denied` | Re-enumerate interface 2 and verify ACLs / udev rules; do not run as `root` |
| Multiple matching controllers | Pass `--device-path` from `hid-list` output |
| Connected but remains offline | Verify report length and permissions via `hid-read` |
| `another producer holds the file lock` | Stop the other running `bridge` or `produce` process; do not delete the lock file while active |
| Protocol version mismatch | Stop all processes, remove `/dev/shm/chuni_bridge_probe`, and restart with matching versions |
| Wine sees no input | Check DLL bitness (`x86` vs `x64`), loader INI config, shared path, and file read/write permissions |
| Wrong or flickering LED colors | Ensure only one process writes LEDs and that colors are not double-permuted |

## Project Layout

```text
.
├── Cargo.toml                   # Rust package manifest & dependencies
├── Cargo.lock                   # Locked dependency graph
├── src/
│   ├── lib.rs                   # Shared-memory IPC layout, atomic snapshots, parsing & LED packets
│   ├── main.rs                  # CLI entry point & shared-memory reader
│   ├── native.rs                # HID enumeration, raw reading & synthetic producer
│   └── native/
│       ├── bridge.rs            # Live input bridge, device rediscovery & LED forwarding
│       └── led.rs               # Synthetic LED emitter & hardware calibration commands
├── windows/                     # Freestanding C11 chuniio DLL, exports & input/LED state logic
├── tools/                       # Build scripts, HID capture, Wine validators & latency probes
├── packaging/udev/              # udev rules for RPunithm interface 2
├── artifacts/                   # Local logs & validation outputs (gitignored)
└── target/                      # Rust, DLL & PE build outputs (gitignored)
```

## Contributing

When opening an issue, please include your Linux distribution, Rust and Wine versions, `hid-list` output, DLL architecture (`x86` or `x64`), reproduction steps, and redacted logs. Changes touching the input or LED wire protocol should clearly distinguish unit tests, synthetic pipeline checks, and operator-verified hardware testing.
