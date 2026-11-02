use anyhow::{bail, ensure, Context, Result};
use chuni_bridge_probe::{game_order, parse_report, InputWriter, Snapshot, SOURCE_SYNTHETIC};
use hidapi::{HidApi, HidDevice};

mod bridge;
pub mod led;
pub use bridge::{run_bridge, BridgeOptions};
pub use led::{run_hid_led, run_led_emit, HidLedOptions, LedEmitOptions, LedPatternArgs};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const VID: u16 = 0x1973;
const PID: u16 = 0x2001;

pub fn hid_list(all: bool) -> Result<()> {
    let api = HidApi::new().context("initialize HIDAPI hidraw backend")?;
    let mut count = 0;
    let mut paths = std::collections::BTreeSet::new();
    for info in api.device_list() {
        let matching = info.vendor_id() == VID && info.product_id() == PID;
        if matching {
            count += 1;
            paths.insert(info.path().to_bytes().to_vec());
        }
        if all || matching {
            println!("vid={:04x} pid={:04x} interface={} usage_page={:04x} usage={:04x} path={} product={:?} serial={:?}{}",
                info.vendor_id(), info.product_id(), info.interface_number(), info.usage_page(), info.usage(),
                info.path().to_string_lossy(), info.product_string(), info.serial_number(),
                if matching && info.interface_number() == 2 { " [slider candidate]" } else { "" });
        }
    }
    println!("matching_device_paths={} matching_entries={count} (VID={VID:04x} PID={PID:04x}); probe does not poll input during enumeration", paths.len());
    Ok(())
}

struct OpenHid {
    device: HidDevice,
    path: String,
    _api: HidApi,
}

fn open_hid(interface: i32, device_path: Option<&str>) -> Result<OpenHid> {
    let api = HidApi::new().context("initialize HIDAPI hidraw backend")?;
    let mut devices: Vec<_> = api
        .device_list()
        .filter(|d| {
            d.vendor_id() == VID
                && d.product_id() == PID
                && d.interface_number() == interface
                && device_path.is_none_or(|p| d.path().to_bytes() == p.as_bytes())
        })
        .collect();
    // HIDAPI can enumerate the same hidraw path once per usage collection.
    // Only distinct paths represent distinct candidate interfaces.
    devices.sort_unstable_by(|a, b| a.path().to_bytes().cmp(b.path().to_bytes()));
    devices.dedup_by(|a, b| a.path() == b.path());
    ensure!(!devices.is_empty(), "no matching {VID:04x}:{PID:04x} interface {interface}; run hid-list --all, connect the controller and check udev permissions");
    ensure!(
        devices.len() == 1,
        "{} matching paths; select one using --device-path from hid-list",
        devices.len()
    );
    let info = devices[0];
    let device = info.open_device(&api).with_context(|| {
        format!(
            "open {} (check hidraw permissions; do not run the game/probe as root)",
            info.path().to_string_lossy()
        )
    })?;
    let path = info.path().to_string_lossy().into_owned();
    Ok(OpenHid {
        device,
        path,
        _api: api,
    })
}

pub fn hid_read(
    interface: i32,
    device_path: Option<&str>,
    reports: u64,
    duration_secs: u64,
    raw: bool,
    running: &AtomicBool,
) -> Result<()> {
    let opened = open_hid(interface, device_path)?;
    let device = &opened.device;
    println!(
        "opened path={} interface={interface}; expecting 45 bytes with NO inserted report-ID byte",
        opened.path
    );
    let start = Instant::now();
    let mut buffer = [0_u8; 512];
    let mut valid = 0;
    let mut invalid = 0;
    while running.load(Ordering::Relaxed)
        && (duration_secs == 0 || start.elapsed() < Duration::from_secs(duration_secs))
        && (reports == 0 || valid < reports)
    {
        let n = device
            .read_timeout(&mut buffer, 20)
            .context("read HID input (device may have disconnected)")?;
        if n == 0 {
            continue;
        }
        if raw || n != 45 {
            println!("raw len={n} bytes={:02x?}", &buffer[..n]);
        }
        match parse_report(&buffer[..n]) {
            Ok(input) => {
                valid += 1;
                println!("report={valid} air_raw={:02x} buttons_raw={:02x} slider_raw={:?} slider_game={:?}",
                    input.air, input.buttons, input.slider, game_order(&input.slider));
            }
            Err(e) => {
                invalid += 1;
                eprintln!("rejected report: {e}");
            }
        }
    }
    println!(
        "summary valid_reports={valid} invalid_reports={invalid} elapsed_ms={}",
        start.elapsed().as_millis()
    );
    ensure!(
        invalid == 0,
        "unexpected report format; inspect raw bytes before using documented offsets"
    );
    if valid == 0 {
        bail!("no valid 45-byte report received; this does NOT validate the hardware protocol");
    }
    Ok(())
}

pub fn produce(
    path: &Path,
    interval_ms: u64,
    duration_secs: u64,
    running: &AtomicBool,
) -> Result<()> {
    ensure!(interval_ms > 0, "interval-ms must be positive");
    let writer = InputWriter::open(path)?;
    let ticks = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let session = (ticks as u32) ^ ((ticks >> 32) as u32) ^ std::process::id();
    let start = Instant::now();
    let mut heartbeat = 0_u32;
    writer.publish(&Snapshot {
        session,
        source: SOURCE_SYNTHETIC,
        ..Snapshot::default()
    });
    println!(
        "producer source=synthetic path={} session={session} interval_ms={interval_ms}",
        path.display()
    );
    println!("synthetic pattern: slider[i] = (heartbeat + i) modulo 256; NOT real controller data");
    while running.load(Ordering::Relaxed)
        && (duration_secs == 0 || start.elapsed() < Duration::from_secs(duration_secs))
    {
        heartbeat = heartbeat.wrapping_add(1);
        writer.publish(&Snapshot::synthetic(session, heartbeat));
        std::thread::sleep(Duration::from_millis(interval_ms));
    }
    heartbeat = heartbeat.wrapping_add(1);
    writer.publish(&Snapshot {
        session,
        heartbeat,
        source: SOURCE_SYNTHETIC,
        ..Snapshot::default()
    });
    println!("producer stopped; published offline/zero, heartbeat={heartbeat}; retained backing file for existing readers");
    Ok(())
}
