//! LED output tools. `led-emit` publishes synthetic game-order LED states into
//! the experimental shared-memory LED region (no USB traffic). `hid-led` sends
//! a calibration pattern DIRECTLY to the device as 61-byte output reports; it
//! is the only probe that writes HID output data, and only on explicit request.

use super::open_hid;
use anyhow::{ensure, Context, Result};
use chuni_bridge_probe::{led_packets, LedState, LedWriter, LED_GROUND_ZONES, LED_PACKET_LEN};
use clap::Args;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

fn parse_color(value: &str) -> Result<[u8; 3]> {
    let hex = u32::from_str_radix(value, 16)
        .with_context(|| format!("color must be 6 hex digits (RRGGBB), got {value:?}"))?;
    ensure!(
        value.len() == 6 && hex <= 0xFF_FFFF,
        "color must be 6 hex digits (RRGGBB)"
    );
    Ok([(hex >> 16) as u8, (hex >> 8) as u8, hex as u8])
}

fn format_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Classic 256-position color wheel for calibration patterns.
fn wheel(position: u8) -> [u8; 3] {
    let position = position % 96;
    let (r, g, b) = match position {
        0..=31 => (255 - position * 8, position * 8, 0),
        32..=63 => (0, 255 - (position - 32) * 8, (position - 32) * 8),
        _ => ((position - 64) * 8, 0, 255 - (position - 64) * 8),
    };
    [r, g, b]
}

#[derive(Args, Debug, Default)]
pub struct LedPatternArgs {
    /// Solid color for every slider zone (RRGGBB).
    #[arg(long, value_parser = parse_color)]
    pub solid: Option<[u8; 3]>,
    /// Light only slider zone N (0-31, game order); requires --color.
    #[arg(long)]
    pub zone: Option<u8>,
    /// Color for --zone (RRGGBB).
    #[arg(long, value_parser = parse_color, requires = "zone")]
    pub color: Option<[u8; 3]>,
    /// Animated color wheel across all zones.
    #[arg(long)]
    pub rainbow: bool,
    /// All LEDs off.
    #[arg(long)]
    pub off: bool,
    /// Compatibility SHM left-air color (RRGGBB); ignored by the ToF device output.
    #[arg(long, value_parser = parse_color)]
    pub air_left: Option<[u8; 3]>,
    /// Compatibility SHM right-air color (RRGGBB); ignored by the ToF device output.
    #[arg(long, value_parser = parse_color)]
    pub air_right: Option<[u8; 3]>,
}

impl LedPatternArgs {
    fn exclusive(&self) -> Result<Pattern> {
        let mut chosen = 0;
        if self.solid.is_some() {
            chosen += 1;
        }
        if self.zone.is_some() {
            chosen += 1;
        }
        if self.rainbow {
            chosen += 1;
        }
        if self.off {
            chosen += 1;
        }
        ensure!(
            chosen == 1,
            "pick exactly one of --solid, --zone, --rainbow or --off (air colors may be combined)"
        );
        if let Some(zone) = self.zone {
            ensure!(zone < LED_GROUND_ZONES as u8, "zone must be 0-31");
            ensure!(self.color.is_some(), "--zone requires --color");
            return Ok(Pattern::Zone(zone, self.color.unwrap()));
        }
        if let Some(color) = self.solid {
            return Ok(Pattern::Solid(color));
        }
        if self.rainbow {
            return Ok(Pattern::Rainbow);
        }
        Ok(Pattern::Off) // --off ignores any air colors entirely.
    }

    fn base_state(&self) -> LedState {
        let mut led = LedState::default();
        if let Some(color) = self.air_left {
            led.air_left = color;
        }
        if let Some(color) = self.air_right {
            led.air_right = color;
        }
        led
    }
}

enum Pattern {
    Solid([u8; 3]),
    Zone(u8, [u8; 3]),
    Rainbow,
    Off,
}

impl Pattern {
    fn frame(&self, base: &LedState, frame: u64) -> LedState {
        let mut led = base.clone();
        match self {
            Self::Solid(color) => {
                for zone in 0..LED_GROUND_ZONES {
                    led.ground[zone * 3..zone * 3 + 3].copy_from_slice(color);
                }
            }
            Self::Zone(zone, color) => {
                led.ground[*zone as usize * 3..*zone as usize * 3 + 3].copy_from_slice(color);
            }
            Self::Rainbow => {
                for zone in 0..LED_GROUND_ZONES {
                    let color = wheel(((zone as u64 * 3 + frame * 2) % 256) as u8);
                    led.ground[zone * 3..zone * 3 + 3].copy_from_slice(&color);
                }
            }
            Self::Off => {}
        }
        led
    }
}

/// Frame the two 61-byte device reports as one unnumbered hidraw output
/// transfer each: a zero report-ID byte followed by the packet payload.
pub fn led_output_reports(packets: &[[u8; LED_PACKET_LEN]; 2]) -> [[u8; LED_PACKET_LEN + 1]; 2] {
    let mut reports = [[0_u8; LED_PACKET_LEN + 1]; 2];
    for (report, packet) in reports.iter_mut().zip(packets) {
        report[1..].copy_from_slice(packet);
    }
    reports
}

pub struct HidLedOptions {
    pub device_path: Option<String>,
    pub hold_ms: u64,
    pub frame_ms: u64,
    pub pattern: LedPatternArgs,
}

/// Writes test patterns straight to interface 2. Explicit opt-in: this is the
/// only command that sends HID output reports to the controller.
pub fn run_hid_led(options: HidLedOptions, running: &AtomicBool) -> Result<()> {
    let pattern = options.pattern.exclusive()?;
    ensure!(options.frame_ms > 0, "frame-ms must be positive");
    let opened = open_hid(2, options.device_path.as_deref())?;
    println!(
        "hid-led path={} writes=hid_output_reports interface=2 hold_ms={} frame_ms={}",
        opened.path, options.hold_ms, options.frame_ms
    );
    println!(
        "pattern: slider RGB passes through; zone 7 is eighth from right on the tested device; zone 31 has no device LED"
    );
    if options.pattern.air_left.is_some() || options.pattern.air_right.is_some() {
        eprintln!(
            "note: this ToF RPunithm has no air LEDs; --air-left/--air-right are not sent to HID"
        );
    }
    let base = options.pattern.base_state();
    if matches!(pattern, Pattern::Off) {
        for report in led_output_reports(&led_packets(&LedState::default())) {
            opened
                .device
                .write(&report)
                .context("write LED output report (device may have disconnected)")?;
        }
        println!("hid-led sent an all-off pattern and exited");
        return Ok(());
    }
    let start = Instant::now();
    let mut frames = 0_u64;
    let mut last = None;
    while running.load(Ordering::Relaxed)
        && (options.hold_ms == 0 || start.elapsed() < Duration::from_millis(options.hold_ms))
    {
        let frame = frames;
        let led = pattern.frame(&base, frame);
        if Some(&led) != last.as_ref() {
            let reports = led_output_reports(&led_packets(&led));
            for report in &reports {
                opened
                    .device
                    .write(report)
                    .context("write LED output report (device may have disconnected)")?;
            }
            last = Some(led);
            frames += 1;
        }
        if matches!(pattern, Pattern::Rainbow) {
            std::thread::sleep(Duration::from_millis(options.frame_ms));
        } else if last.is_some() {
            // Static pattern: nothing more to send until shutdown.
            std::thread::sleep(Duration::from_millis(50));
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    // Always leave the device dark on ordinary exit; a killed process cannot.
    let mut off = base;
    off.ground.fill(0);
    off.air_left = [0; 3];
    off.air_right = [0; 3];
    for report in led_output_reports(&led_packets(&off)) {
        if let Err(error) = opened.device.write(&report) {
            println!("hid-led off-write failed: {error}");
        }
    }
    println!("hid-led stopped; sent {frames} distinct LED frames plus an off frame");
    Ok(())
}

pub struct LedEmitOptions {
    pub path: PathBuf,
    pub duration_secs: u64,
    pub interval_ms: u64,
    pub pattern: LedPatternArgs,
}

/// Publishes a synthetic LED state through the LED region, like the Wine DLL
/// would. The bridge (or any reader) observes it; no USB traffic happens here.
pub fn run_led_emit(options: LedEmitOptions, running: &AtomicBool) -> Result<()> {
    let pattern = options.pattern.exclusive()?;
    ensure!(options.interval_ms > 0, "interval-ms must be positive");
    let writer = LedWriter::open(&options.path)?;
    let base = options.pattern.base_state();
    println!(
        "led-emit path={} interval_ms={} pattern={:?} air_left={} air_right={}",
        options.path.display(),
        options.interval_ms,
        options.pattern,
        format_hex(&base.air_left),
        format_hex(&base.air_right)
    );
    println!("led-emit publishes the same framing the Wine DLL uses; on exit it publishes an all-zero state");
    let start = Instant::now();
    let mut frames = 0_u64;
    while running.load(Ordering::Relaxed)
        && (options.duration_secs == 0
            || start.elapsed() < Duration::from_secs(options.duration_secs))
    {
        let led = pattern.frame(&base, frames);
        writer.publish(&led);
        frames += 1;
        std::thread::sleep(Duration::from_millis(options.interval_ms));
    }
    writer.publish(&LedState::default()); // Lights off via the bridge.
    println!(
        "led-emit stopped after {frames} frames; published final zero state; retained backing file"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(args: &[&str]) -> LedPatternArgs {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            pattern: LedPatternArgs,
        }
        Cli::parse_from(std::iter::once("prog").chain(args.iter().copied())).pattern
    }

    #[test]
    fn pattern_rules_and_color_parsing() {
        assert!(pattern(&["--solid", "ff0000"]).exclusive().is_ok());
        assert!(pattern(&["--zone", "5", "--color", "00ff00"])
            .exclusive()
            .is_ok());
        assert!(pattern(&["--off"]).exclusive().is_ok());
        assert!(pattern(&[]).exclusive().is_err());
        assert!(pattern(&["--solid", "ff0000", "--rainbow"])
            .exclusive()
            .is_err());
        assert!(pattern(&["--zone", "32", "--color", "ff0000"])
            .exclusive()
            .is_err());
        assert!(pattern(&["--zone", "5"]).exclusive().is_err());
        assert_eq!(parse_color("0A0b0c").unwrap(), [0x0A, 0x0B, 0x0C]);
        assert!(parse_color("fff").is_err());
        assert!(parse_color("1234567").is_err());
        assert!(parse_color("zzzzzz").is_err());
        let air = pattern(&[
            "--solid",
            "112233",
            "--air-left",
            "445566",
            "--air-right",
            "778899",
        ]);
        let base = air.base_state();
        assert_eq!(base.air_left, [0x44, 0x55, 0x66]);
        assert_eq!(base.air_right, [0x77, 0x88, 0x99]);
    }

    #[test]
    fn frames_and_output_report_framing() {
        let zone = pattern(&["--zone", "7", "--color", "123456"]);
        let state = zone.exclusive().unwrap().frame(&zone.base_state(), 0);
        assert_eq!(state.ground[7 * 3..7 * 3 + 3], [0x12, 0x34, 0x56]);
        assert!(state
            .ground
            .iter()
            .enumerate()
            .all(|(i, &b)| { (7 * 3..7 * 3 + 3).contains(&i) || b == 0 }));
        let rainbow = pattern(&["--rainbow"]);
        let first = rainbow.exclusive().unwrap().frame(&rainbow.base_state(), 0);
        let second = rainbow
            .exclusive()
            .unwrap()
            .frame(&rainbow.base_state(), 10);
        assert_ne!(first.ground, second.ground);
        let reports = led_output_reports(&led_packets(&state));
        for report in &reports {
            assert_eq!(report.len(), LED_PACKET_LEN + 1);
            assert_eq!(report[0], 0); // Zero report-ID byte for unnumbered reports.
        }
    }
}
