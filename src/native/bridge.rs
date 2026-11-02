//! Experimental real HID producer. Only interface 2 is opened; retries always
//! rediscover its path. Input reports are one-way, but this prototype also
//! forwards the shared-memory LED region to the device as 61-byte output
//! reports (YubiDeck protocol) whenever a new coherent LED publication appears.

use super::led::led_output_reports;
use super::{open_hid, OpenHid};
use anyhow::{ensure, Result};
use chuni_bridge_probe::{led_packets, parse_report, InputWriter, LedState, Snapshot, SOURCE_HID};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(20);
const READ_TIMEOUT_MS: i32 = 20;

pub struct BridgeOptions {
    pub path: PathBuf,
    pub device_path: Option<String>,
    pub duration_secs: u64,
    pub retry_ms: u64,
    pub report_timeout_ms: u64,
    /// Forward LED publications from the SHM LED region to the device.
    pub leds: bool,
    pub trace: bool,
}

/// Device health is separate from process liveness: publishing heartbeats must
/// never extend the lifetime of an old pressed report.
struct HidInput {
    snapshot: Snapshot,
    report_anchor: Option<Instant>,
}

impl HidInput {
    fn new(session: u32) -> Self {
        Self {
            snapshot: Snapshot {
                session,
                source: SOURCE_HID,
                ..Snapshot::default()
            },
            report_anchor: None,
        }
    }

    fn offline(&mut self) {
        self.snapshot = Snapshot {
            session: self.snapshot.session,
            heartbeat: self.snapshot.heartbeat,
            source: SOURCE_HID,
            ..Snapshot::default()
        };
        self.report_anchor = None;
    }

    fn connected(&mut self, now: Instant) {
        self.offline();
        // Also bound the wait for the first valid report after open.
        self.report_anchor = Some(now);
    }

    fn accept(&mut self, report: &[u8], now: Instant) -> Result<()> {
        let parsed = match parse_report(report) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.offline();
                return Err(error);
            }
        };
        self.snapshot = Snapshot {
            session: self.snapshot.session,
            heartbeat: self.snapshot.heartbeat,
            ..parsed
        };
        self.report_anchor = Some(now);
        Ok(())
    }

    fn expire(&mut self, now: Instant, timeout: Duration) -> bool {
        if self
            .report_anchor
            .is_some_and(|anchor| now.duration_since(anchor) >= timeout)
        {
            self.offline();
            true
        } else {
            false
        }
    }

    fn next_snapshot(&mut self) -> Snapshot {
        self.snapshot.heartbeat = self.snapshot.heartbeat.wrapping_add(1);
        self.snapshot.clone()
    }
}

struct Publisher {
    writer: InputWriter,
    input: HidInput,
    trace: bool,
    finished: bool,
}

impl Publisher {
    fn publish(&mut self) {
        let snapshot = self.input.next_snapshot();
        if self.trace {
            // Record BEFORE publication: any live Wine sample has a prior trace
            // record, even if the producer is killed immediately after publish.
            // Only input fields are logged, never card bytes 34..44.
            println!(
                "sample session={} heartbeat={} online={} source={} air={} buttons={} slider={:?}",
                snapshot.session,
                snapshot.heartbeat,
                u8::from(snapshot.online),
                snapshot.source,
                snapshot.air,
                snapshot.buttons,
                snapshot.slider
            );
        }
        self.writer.publish(&snapshot);
    }

    fn finish(&mut self) {
        self.input.offline();
        self.publish();
        self.finished = true;
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        if !self.finished {
            // Best-effort safety on early return/unwind; no logging that could
            // itself panic. SIGKILL is handled by the reader's bounded fallback.
            self.input.offline();
            self.writer.publish(&self.input.next_snapshot());
        }
    }
}

fn status(previous: &mut String, message: String) {
    if *previous != message {
        println!("bridge {message}");
        *previous = message;
    }
}

pub fn run_bridge(options: BridgeOptions, running: &AtomicBool) -> Result<()> {
    ensure!(options.retry_ms > 0, "retry-ms must be positive");
    ensure!(
        options.report_timeout_ms > 0,
        "report-timeout-ms must be positive"
    );
    let retry = Duration::from_millis(options.retry_ms);
    let report_timeout = Duration::from_millis(options.report_timeout_ms);
    let ticks = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let session = (ticks as u32) ^ ((ticks >> 32) as u32) ^ std::process::id();
    let mut publisher = Publisher {
        writer: InputWriter::open(&options.path)?,
        input: HidInput::new(session),
        trace: options.trace,
        finished: false,
    };
    publisher.publish();
    println!(
        "bridge source=hid path={} session={session} interface=2 retry_ms={} report_timeout_ms={} leds={}",
        options.path.display(),
        options.retry_ms,
        options.report_timeout_ms,
        if options.leds { "forwarded" } else { "disabled" }
    );
    let start = Instant::now();
    let mut last_publish = start;
    let mut next_open = start;
    let mut opened: Option<OpenHid> = None;
    let mut previous_status = String::new();
    let mut valid = 0_u64;
    let mut invalid = 0_u64;
    let mut attempts = 0_u64;
    let mut connections = 0_u64;
    let mut timeouts = 0_u64;
    // Last LED state forwarded to the device. Reset to None on (re)connect so
    // the current state is always re-pushed after a device or power cycle.
    let mut led_forwarded: Option<LedState> = None;
    let mut led_writes = 0_u64;
    let mut led_dropped = 0_u64;
    let mut buffer = [0_u8; 512];

    while running.load(Ordering::Relaxed)
        && (options.duration_secs == 0
            || start.elapsed() < Duration::from_secs(options.duration_secs))
    {
        if opened.is_none() && Instant::now() >= next_open {
            attempts += 1;
            match open_hid(2, options.device_path.as_deref()) {
                Ok(device) => {
                    publisher.input.connected(Instant::now());
                    connections += 1;
                    led_forwarded = None; // Re-push the current LED state.
                    status(
                        &mut previous_status,
                        format!(
                            "state=connected path={} waiting_for_valid_report",
                            device.path
                        ),
                    );
                    opened = Some(device);
                }
                Err(error) => {
                    publisher.input.offline();
                    status(
                        &mut previous_status,
                        format!("state=offline reason={error:#}"),
                    );
                    next_open = Instant::now() + retry;
                }
            }
        }
        if let Some(device) = &opened {
            let result = device.device.read_timeout(&mut buffer, READ_TIMEOUT_MS);
            // SIGINT/SIGTERM may interrupt hidraw poll/read. Do not diagnose
            // the requested shutdown as a device fault (HIDAPI can say "Success").
            if !running.load(Ordering::Relaxed) {
                break;
            }
            let now = Instant::now();
            let mut problem = match result {
                Ok(0) => {
                    if publisher.input.expire(now, report_timeout) {
                        timeouts += 1;
                        Some("no valid HID report within report-timeout-ms".to_owned())
                    } else {
                        None
                    }
                }
                Ok(n) => match publisher.input.accept(&buffer[..n], now) {
                    Ok(()) => {
                        valid += 1;
                        status(
                            &mut previous_status,
                            format!("state=online path={}", device.path),
                        );
                        publisher.publish();
                        last_publish = Instant::now();
                        None
                    }
                    Err(error) => {
                        invalid += 1;
                        Some(format!("invalid input report: {error:#}"))
                    }
                },
                Err(error) => Some(format!("HID read failed: {error}")),
            };
            // Forward at most one LED update per loop iteration, coalescing
            // faster game publications. A write failure is treated like a read
            // failure: fail closed, rediscover, re-push LEDs after reconnect.
            if options.leds && problem.is_none() {
                if let Some(led) = publisher.writer.led() {
                    if Some(&led) != led_forwarded.as_ref() {
                        let reports = led_output_reports(&led_packets(&led));
                        match device
                            .device
                            .write(&reports[0])
                            .and_then(|_| device.device.write(&reports[1]))
                        {
                            Ok(_) => {
                                led_writes += 1;
                                led_forwarded = Some(led);
                            }
                            Err(error) => problem = Some(format!("HID LED write failed: {error}")),
                        }
                    }
                }
            }
            if let Some(problem) = problem {
                publisher.input.offline();
                publisher.publish(); // Clear immediately, not at the retry deadline.
                last_publish = Instant::now();
                opened = None;
                next_open = last_publish + retry;
                status(
                    &mut previous_status,
                    format!("state=offline reason={problem}"),
                );
            }
        } else {
            // While offline, keep observing LED publications so the summary
            // can report dropped updates; the data itself is simply discarded.
            if options.leds {
                if let Some(led) = publisher.writer.led() {
                    if Some(&led) != led_forwarded.as_ref() {
                        led_forwarded = Some(led);
                        led_dropped += 1;
                    }
                }
            }
            // Keep advancing offline heartbeats while waiting for hardware or
            // permission recovery. Never busy-loop or poll unrelated inputs.
            std::thread::sleep(HEARTBEAT_INTERVAL);
        }
        if last_publish.elapsed() >= HEARTBEAT_INTERVAL {
            publisher.publish();
            last_publish = Instant::now();
        }
    }
    drop(opened);
    publisher.finish();
    println!("bridge stopped; published offline/zero; retained backing file for existing readers");
    println!("summary valid_reports={valid} invalid_reports={invalid} open_attempts={attempts} connections={connections} report_timeouts={timeouts} led_writes={led_writes} led_dropped={led_dropped} elapsed_ms={}",
        start.elapsed().as_millis());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pressed_report() -> [u8; 45] {
        let mut report = [0; 45];
        report[0] = 63;
        report[1] = 7;
        report[2..34].fill(255);
        report[34..].fill(123); // Must never be copied into the IPC payload.
        report
    }

    fn assert_offline(snapshot: &Snapshot) {
        assert!(!snapshot.online);
        assert_eq!(snapshot.source, SOURCE_HID);
        assert_eq!(snapshot.air, 0);
        assert_eq!(snapshot.buttons, 0);
        assert_eq!(snapshot.slider, [0; 32]);
    }

    #[test]
    fn only_valid_report_brings_device_online() {
        let mut input = HidInput::new(42);
        assert_offline(&input.next_snapshot());
        input.connected(Instant::now());
        assert_offline(&input.next_snapshot());
        input.accept(&pressed_report(), Instant::now()).unwrap();
        let sample = input.next_snapshot();
        assert_eq!((sample.session, sample.heartbeat), (42, 3));
        assert!(sample.online);
        assert_eq!((sample.air, sample.buttons), (63, 7));
        assert_eq!(sample.slider, [255; 32]);
    }

    #[test]
    fn heartbeats_do_not_keep_a_stale_pressed_report_alive() {
        let now = Instant::now();
        let mut input = HidInput::new(1);
        input.connected(now);
        input.accept(&pressed_report(), now).unwrap();
        let timeout = Duration::from_millis(100);
        for ms in [20, 40, 60, 80] {
            assert!(!input.expire(now + Duration::from_millis(ms), timeout));
            assert!(input.next_snapshot().online);
        }
        assert!(input.expire(now + timeout, timeout));
        assert_offline(&input.next_snapshot());
    }

    #[test]
    fn fresh_reports_extend_deadline_and_release_replaces_all_input_fields() {
        let now = Instant::now();
        let mut input = HidInput::new(7);
        input.accept(&pressed_report(), now).unwrap();
        input
            .accept(&[0; 45], now + Duration::from_millis(90))
            .unwrap();
        let timeout = Duration::from_millis(100);
        assert!(!input.expire(now + Duration::from_millis(110), timeout));
        let released = input.next_snapshot();
        assert!(released.online);
        assert_eq!((released.air, released.buttons), (0, 0));
        assert_eq!(released.slider, [0; 32]);
        assert!(input.expire(now + Duration::from_millis(190), timeout));
        assert_offline(&input.next_snapshot());
    }

    #[test]
    fn first_report_timeout_and_invalid_report_are_fail_closed() {
        let now = Instant::now();
        let mut input = HidInput::new(1);
        input.connected(now);
        assert!(input.expire(now + Duration::from_millis(100), Duration::from_millis(100)));
        assert_offline(&input.next_snapshot());
        for len in [0, 44, 46, 64] {
            input.connected(now);
            input.accept(&pressed_report(), now).unwrap();
            assert!(input.accept(&vec![0; len], now).is_err());
            assert_offline(&input.next_snapshot());
        }
    }

    #[test]
    fn reconnect_does_not_replay_previous_press_and_heartbeat_wraps() {
        let now = Instant::now();
        let mut input = HidInput::new(7);
        input.accept(&pressed_report(), now).unwrap();
        input.offline();
        input.connected(now);
        assert_offline(&input.next_snapshot());
        input.accept(&[0; 45], now).unwrap();
        input.snapshot.heartbeat = u32::MAX;
        let sample = input.next_snapshot();
        assert!(sample.online);
        assert_eq!(sample.slider, [0; 32]);
        assert_eq!((sample.session, sample.heartbeat), (7, 0));
    }

    #[test]
    fn publisher_drop_clears_pressed_mapping_and_releases_writer_lock() {
        let directory =
            std::env::temp_dir().join(format!("chuni-hid-drop-test-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("input");
        let writer = InputWriter::open(&path).unwrap();
        let mapping = chuni_bridge_probe::ReadMapping::open(&path).unwrap();
        {
            let mut publisher = Publisher {
                writer,
                input: HidInput::new(9),
                trace: false,
                finished: false,
            };
            publisher
                .input
                .accept(&pressed_report(), Instant::now())
                .unwrap();
            publisher.publish();
            assert!(mapping.snapshot().unwrap().online);
            // No explicit finish: exercise the early-return/unwind drop path.
        }
        assert_offline(&mapping.snapshot().unwrap());
        drop(InputWriter::open(&path).unwrap());
        drop(mapping);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
