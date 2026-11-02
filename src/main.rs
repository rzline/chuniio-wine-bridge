use anyhow::{ensure, Context, Result};
use chuni_bridge_probe::{InputState, Liveness, ReadMapping};
use clap::{Parser, Subcommand};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
mod native;
#[cfg(target_os = "linux")]
use native::LedPatternArgs;

#[cfg(windows)]
const DEFAULT_PATH: &str = r"Z:\dev\shm\chuni_bridge_probe";
#[cfg(not(windows))]
const DEFAULT_PATH: &str = "/dev/shm/chuni_bridge_probe";

#[derive(Parser)]
#[command(
    version,
    about = "Minimal RPunithm HID and Linux↔Wine mmap probes (NOT a chuniio DLL)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Enumerate VID 1973 / PID 2001 without polling input reports.
    #[cfg(target_os = "linux")]
    HidList {
        /// Also show non-RPunithm HID devices.
        #[arg(long)]
        all: bool,
    },
    /// Read raw reports from a matching slider interface. Never writes USB data.
    #[cfg(target_os = "linux")]
    HidRead {
        #[arg(long, default_value_t = 2)]
        interface: i32,
        /// Select one ENUMERATED matching device path if multiple are present.
        #[arg(long)]
        device_path: Option<String>,
        #[arg(long, default_value_t = 20)]
        reports: u64,
        /// Maximum duration; 0 = until Ctrl+C/report limit.
        #[arg(long, default_value_t = 10)]
        duration_secs: u64,
        /// Include the complete bytes as returned by HIDAPI.
        #[arg(long)]
        raw: bool,
    },
    /// Publish a deterministic synthetic input ramp into an experimental SHM file.
    #[cfg(target_os = "linux")]
    Produce {
        #[arg(long, default_value = DEFAULT_PATH)]
        path: PathBuf,
        #[arg(long, default_value_t = 10)]
        interval_ms: u64,
        /// 0 = until Ctrl+C; ordinary shutdown publishes zero/offline.
        #[arg(long, default_value_t = 0)]
        duration_secs: u64,
    },
    /// Bridge real interface-2 HID reports into experimental SHM, with offline retries.
    #[cfg(target_os = "linux")]
    Bridge {
        #[arg(long, default_value = DEFAULT_PATH)]
        path: PathBuf,
        /// Optional diagnostic pin; default rediscovery survives hidraw renumbering.
        #[arg(long)]
        device_path: Option<String>,
        /// 0 = until Ctrl+C/SIGTERM; ordinary shutdown publishes zero/offline.
        #[arg(long, default_value_t = 0)]
        duration_secs: u64,
        #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(1..=60_000))]
        retry_ms: u64,
        /// Clear/drop a device with no valid reports; independent of process heartbeat.
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..=1000))]
        report_timeout_ms: u64,
        /// Forward the SHM LED region to the device as HID output reports.
        #[arg(long)]
        no_leds: bool,
        /// Log each published input snapshot for cross-process verification; no card data.
        #[arg(long)]
        trace: bool,
    },
    /// Send a calibration LED pattern DIRECTLY to interface 2 (writes HID output).
    #[cfg(target_os = "linux")]
    HidLed {
        /// Select one ENUMERATED matching device path if multiple are present.
        #[arg(long)]
        device_path: Option<String>,
        /// How long to keep the pattern; 0 = until Ctrl+C. Static patterns hold, --rainbow animates.
        #[arg(long, default_value_t = 3000, value_parser = clap::value_parser!(u64).range(0..=3_600_000))]
        hold_ms: u64,
        /// Animation frame time for --rainbow.
        #[arg(long, default_value_t = 33, value_parser = clap::value_parser!(u64).range(10..=1000))]
        frame_ms: u64,
        #[command(flatten)]
        pattern: LedPatternArgs,
    },
    /// Publish a synthetic game-order LED state into the SHM LED region (no USB writes).
    #[cfg(target_os = "linux")]
    LedEmit {
        #[arg(long, default_value = DEFAULT_PATH)]
        path: PathBuf,
        /// 0 = until Ctrl+C; ordinary shutdown publishes an all-zero LED state.
        #[arg(long, default_value_t = 0)]
        duration_secs: u64,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u64).range(1..=1000))]
        interval_ms: u64,
        #[command(flatten)]
        pattern: LedPatternArgs,
    },
    /// Read and validate mmap snapshots. This same command builds as a Windows EXE.
    Read {
        #[arg(long, default_value = DEFAULT_PATH)]
        path: PathBuf,
        /// 0 = until Ctrl+C.
        #[arg(long, default_value_t = 10)]
        duration_secs: u64,
        #[arg(long, default_value_t = 5)]
        poll_ms: u64,
        #[arg(long, default_value_t = 300)]
        stale_ms: u64,
        /// Validate every coherent payload against the producer's ramp pattern.
        #[arg(long)]
        verify_synthetic: bool,
        /// Fail unless at least one heartbeat-confirmed live snapshot was seen.
        #[arg(long)]
        require_live: bool,
        /// Print LED-region publications (game-order colors forwarded by the bridge).
        #[arg(long)]
        led: bool,
        /// Fail unless at least one LED publication (gen >= 1) was coherently observed.
        #[arg(long)]
        require_led: bool,
    },
}

struct ReaderOptions {
    path: PathBuf,
    duration_secs: u64,
    poll_ms: u64,
    stale_ms: u64,
    verify_synthetic: bool,
    require_live: bool,
    led: bool,
    require_led: bool,
}

fn read(options: ReaderOptions, running: &AtomicBool) -> Result<()> {
    let ReaderOptions {
        path,
        duration_secs,
        poll_ms,
        stale_ms,
        verify_synthetic,
        require_live,
        led: show_led,
        require_led,
    } = options;
    ensure!(
        poll_ms > 0 && stale_ms > 0,
        "poll-ms and stale-ms must be positive"
    );
    let mut mapping: Option<ReadMapping> = None;
    let mut liveness = Liveness::default();
    let start = Instant::now();
    let mut next_open = start;
    let mut last_print = start;
    let mut last_status = String::new();
    let mut snapshots = 0_u64;
    let mut live_samples = 0_u64;
    let mut busy_reads = 0_u64;
    let mut led_generation: Option<u32> = None;
    let mut led_samples = 0_u64;
    let mut led_changes = 0_u64;
    let mut led_publications = 0_u64;
    println!(
        "reader platform={} path={} stale_ms={stale_ms}",
        std::env::consts::OS,
        path.display()
    );
    while running.load(Ordering::Relaxed)
        && (duration_secs == 0 || start.elapsed() < Duration::from_secs(duration_secs))
    {
        let now = Instant::now();
        if mapping.is_none() && now >= next_open {
            match ReadMapping::open(&path) {
                Ok(m) => {
                    println!(
                        "mapped {} bytes; waiting for a heartbeat change",
                        chuni_bridge_probe::REGION_SIZE
                    );
                    mapping = Some(m);
                }
                Err(error) => {
                    let status = format!("unavailable: {error:#}");
                    if status != last_status {
                        println!("state=unavailable slider=[0;32] reason={error:#}");
                        last_status = status;
                    }
                }
            }
            next_open = now + Duration::from_millis(200);
        }
        if let Some(m) = &mapping {
            if let Some(snapshot) = m.snapshot() {
                snapshots += 1;
                if verify_synthetic {
                    snapshot.verify_synthetic()?;
                }
                let (state, safe) =
                    liveness.filter(&snapshot, now, Duration::from_millis(stale_ms));
                if state == InputState::Live {
                    live_samples += 1;
                }
                let status = state.to_string();
                if status != last_status
                    || now.duration_since(last_print) >= Duration::from_millis(250)
                {
                    println!("state={state} session={} heartbeat={} source={} air={:02x} buttons={:02x} slider={:?}",
                        safe.session, safe.heartbeat, safe.source, safe.air, safe.buttons, safe.slider);
                    last_print = now;
                    last_status = status;
                }
            } else {
                busy_reads += 1;
                // A killed producer may leave an odd sequence forever. Do not
                // preserve the previous pressed sample or spin indefinitely.
                if last_status != "busy" {
                    println!(
                        "state=busy slider=[0;32] reason=no coherent snapshot within 3 attempts"
                    );
                    last_status = "busy".into();
                }
            }
            if show_led || require_led {
                if let Some(state) = m.led() {
                    led_samples += 1;
                    let generation = m.led_generation();
                    if generation >= 1 {
                        led_publications += 1;
                    }
                    if led_generation != Some(generation) {
                        led_changes += 1;
                        led_generation = Some(generation);
                        if show_led {
                            let zones = state
                                .ground
                                .as_chunks::<3>()
                                .0
                                .iter()
                                .map(|c| format!("{:02x}{:02x}{:02x}", c[0], c[1], c[2]))
                                .collect::<Vec<_>>()
                                .join(",");
                            println!(
                                "led gen={generation} air_left={:02x}{:02x}{:02x} air_right={:02x}{:02x}{:02x} ground={zones}",
                                state.air_left[0], state.air_left[1], state.air_left[2],
                                state.air_right[0], state.air_right[1], state.air_right[2]
                            );
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(poll_ms));
    }
    println!("summary snapshots={snapshots} live_samples={live_samples} busy_reads={busy_reads} led_samples={led_samples} led_changes={led_changes} led_publications={led_publications} elapsed_ms={}", start.elapsed().as_millis());
    if require_live || verify_synthetic {
        ensure!(
            live_samples > 0,
            "no heartbeat-confirmed live snapshot received"
        );
    }
    if require_led {
        ensure!(
            led_publications > 0,
            "no LED publication (gen >= 1) coherently observed in the LED region"
        );
    }
    Ok(())
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    chuni_bridge_probe::ensure_supported_platform()?;
    let running = Arc::new(AtomicBool::new(true));
    let handler = running.clone();
    ctrlc::set_handler(move || handler.store(false, Ordering::Relaxed))
        .context("install Ctrl+C handler")?;
    match cli.command {
        #[cfg(target_os = "linux")]
        Command::HidList { all } => native::hid_list(all),
        #[cfg(target_os = "linux")]
        Command::HidRead {
            interface,
            device_path,
            reports,
            duration_secs,
            raw,
        } => native::hid_read(
            interface,
            device_path.as_deref(),
            reports,
            duration_secs,
            raw,
            &running,
        ),
        #[cfg(target_os = "linux")]
        Command::Produce {
            path,
            interval_ms,
            duration_secs,
        } => native::produce(&path, interval_ms, duration_secs, &running),
        #[cfg(target_os = "linux")]
        Command::Bridge {
            path,
            device_path,
            duration_secs,
            retry_ms,
            report_timeout_ms,
            no_leds,
            trace,
        } => native::run_bridge(
            native::BridgeOptions {
                path,
                device_path,
                duration_secs,
                retry_ms,
                report_timeout_ms,
                leds: !no_leds,
                trace,
            },
            &running,
        ),
        #[cfg(target_os = "linux")]
        Command::HidLed {
            device_path,
            hold_ms,
            frame_ms,
            pattern,
        } => native::run_hid_led(
            native::HidLedOptions {
                device_path,
                hold_ms,
                frame_ms,
                pattern,
            },
            &running,
        ),
        #[cfg(target_os = "linux")]
        Command::LedEmit {
            path,
            duration_secs,
            interval_ms,
            pattern,
        } => native::run_led_emit(
            native::LedEmitOptions {
                path,
                duration_secs,
                interval_ms,
                pattern,
            },
            &running,
        ),
        Command::Read {
            path,
            duration_secs,
            poll_ms,
            stale_ms,
            verify_synthetic,
            require_live,
            led,
            require_led,
        } => read(
            ReaderOptions {
                path,
                duration_secs,
                poll_ms,
                stale_ms,
                verify_synthetic,
                require_live,
                led,
                require_led,
            },
            &running,
        ),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
