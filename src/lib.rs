//! Experimental probe protocol, not the final chuniio ABI.
//! All concurrently accessed words are atomic, including the payload. This is
//! deliberately restricted to x86/x86_64 Linux and Windows/Wine on one machine.

use anyhow::{bail, ensure, Context, Result};
use memmap2::{MmapOptions, MmapRaw};
use std::{
    fs::File,
    os::fd::AsRawFd,
    path::Path,
    sync::atomic::{AtomicU32, Ordering::SeqCst},
    time::{Duration, Instant},
};

pub const REGION_SIZE: usize = 4096;
pub const MAGIC: u32 = 0x4350_4231; // Probe-only magic, numeric value on little endian.
pub const VERSION: u32 = 2;
pub const SOURCE_SYNTHETIC: u32 = 1;
pub const SOURCE_HID: u32 = 2;
const SEQ: usize = 16;
const SESSION: usize = 17;
const HEARTBEAT: usize = 18;
const ONLINE: usize = 19;
const SOURCE: usize = 20;
const AUX: usize = 21;
const SLIDER: usize = 24;
// LED region (protocol v2), written by the Wine-side DLL, read by the Linux
// bridge. Words are absolute indices into the shared region.
const LED_SEQ: usize = 32;
const LED_GEN: usize = 33;
const LED_GROUND: usize = 34; // 24 words = 96 device-ready bytes in zone order.
const LED_AIR_LEFT: usize = 58;
const LED_AIR_RIGHT: usize = 59;
/// Slider LED zones passed by the game (chuni_io_slider_set_leds payload).
pub const LED_GROUND_ZONES: usize = 32;
/// Physical slider LEDs on the device: 20 in packet 0 plus 11 in packet 1.
pub const LED_SLOTS: usize = 31;
/// Device output report length; the descriptor declares it unnumbered, so a
/// hidraw write must prefix a zero report-ID byte (62 bytes total).
pub const LED_PACKET_LEN: usize = 61;
pub const LED_PACKETS: usize = 2;

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
compile_error!("The probe's interprocess atomic contract is only audited for x86/x86_64");

#[repr(C, align(64))]
struct Region {
    words: [AtomicU32; REGION_SIZE / 4],
}

const _: () = assert!(std::mem::size_of::<Region>() == REGION_SIZE);
const _: () = assert!(std::mem::size_of::<AtomicU32>() == 4);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub session: u32,
    pub heartbeat: u32,
    pub online: bool,
    pub source: u32,
    pub air: u8,
    pub buttons: u8,
    pub slider: [u8; 32],
}

impl Snapshot {
    pub fn synthetic(session: u32, heartbeat: u32) -> Self {
        Self {
            session,
            heartbeat,
            online: true,
            source: SOURCE_SYNTHETIC,
            slider: std::array::from_fn(|i| heartbeat.wrapping_add(i as u32) as u8),
            ..Self::default()
        }
    }

    pub fn verify_synthetic(&self) -> Result<()> {
        if !self.online {
            ensure!(
                self.slider == [0; 32],
                "offline snapshot contains nonzero pressures"
            );
            return Ok(());
        }
        ensure!(self.source == SOURCE_SYNTHETIC, "not a synthetic producer");
        ensure!(
            self.slider == Self::synthetic(self.session, self.heartbeat).slider,
            "inconsistent synthetic snapshot at heartbeat {}",
            self.heartbeat
        );
        Ok(())
    }
}

/// Parse the unnumbered, exactly 45-byte report described by the reference.
/// Do not guess at a report-ID offset or silently accept truncated reports.
pub fn parse_report(report: &[u8]) -> Result<Snapshot> {
    ensure!(
        report.len() == 45,
        "expected 45 bytes, got {}; inspect raw data/report ID before changing offsets",
        report.len()
    );
    let mut slider = [0; 32];
    slider.copy_from_slice(&report[2..34]);
    Ok(Snapshot {
        online: true,
        source: SOURCE_HID,
        air: report[0],
        buttons: report[1],
        slider,
        ..Snapshot::default()
    })
}

/// Keep raw device order in IPC. Apply this permutation only at the game boundary.
pub fn game_order(raw: &[u8; 32]) -> [u8; 32] {
    let mut out = [0; 32];
    for (i, value) in raw.iter().enumerate() {
        out[if i % 2 == 0 { 30 - i } else { 32 - i }] = *value;
    }
    out
}

/// Zone-ordered LED bytes ready for device output.
/// The current Wine DLL trial converts game GBR triples to device BRG once.
/// Linux copies those prepared bytes without any further color permutation.
/// Synthetic calibration publishers supply their own output-order bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedState {
    pub ground: [u8; LED_GROUND_ZONES * 3],
    /// Compatibility-only SHM colors; the ToF RPunithm has no air LEDs.
    pub air_left: [u8; 3],
    /// Compatibility-only SHM colors; not forwarded to the ToF RPunithm.
    pub air_right: [u8; 3],
}

impl Default for LedState {
    fn default() -> Self {
        Self {
            ground: [0; LED_GROUND_ZONES * 3],
            air_left: [0; 3],
            air_right: [0; 3],
        }
    }
}

/// Assemble the two 61-byte YubiDeck-protocol output reports from a
/// game-order LED state.
///
/// Game zone `g` drives device slot `g` directly; zone 31 has no physical
/// LED. On the tested RPunithm, zone 7 lights the eighth LED from the right
/// and zone 27 lights the fourth LED from the left; the operator confirmed
/// 31 slider LEDs and corrected red/green at these positions. Other positions
/// remain to be calibrated; do not infer an input-channel permutation.
/// Slider bytes stay exactly as published: current game trials prepare BRG
/// in the DLL; earlier synthetic RGB hardware tests used RGB bytes directly.
/// Packet indices are 0 and 1 per the firmware emulator and slidershim.
/// Packet 1 slider output is hardware-confirmed. This RPunithm uses ToF air
/// sensing and has no air LEDs: compatibility air colors stay in SHM only,
/// and all non-slider bytes in packet 1 remain zero.
pub fn led_packets(led: &LedState) -> [[u8; LED_PACKET_LEN]; LED_PACKETS] {
    let mut packets = [[0_u8; LED_PACKET_LEN]; LED_PACKETS];
    packets[1][0] = 1; // Packet 0 carries index 0.
    for slot in 0..LED_SLOTS {
        let zone = led.ground.as_chunks::<3>().0[slot];
        let (packet, offset) = if slot < 20 {
            (&mut packets[0], 1 + slot * 3)
        } else {
            (&mut packets[1], 1 + (slot - 20) * 3)
        };
        // Copy device-ready bytes; never duplicate the DLL color permutation.
        packet[offset..offset + 3].copy_from_slice(&zone);
    }
    // No physical air LEDs on the ToF RPunithm. Leave the compatibility
    // air/card positions and remaining padding zero regardless of SHM colors.
    packets
}

impl Region {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.words[0].load(SeqCst) == MAGIC,
            "probe magic mismatch (or initialization incomplete)"
        );
        ensure!(
            self.words[1].load(SeqCst) == VERSION,
            "probe version mismatch"
        );
        ensure!(
            self.words[2].load(SeqCst) == REGION_SIZE as u32,
            "probe size header mismatch"
        );
        Ok(())
    }

    #[cfg(any(target_os = "linux", test))]
    fn initialize(&self) {
        self.words[1].store(VERSION, SeqCst);
        self.words[2].store(REGION_SIZE as u32, SeqCst);
        self.words[0].store(MAGIC, SeqCst); // Publish header last.
    }

    #[cfg(any(target_os = "linux", test))]
    fn publish(&self, input: &Snapshot) {
        // Recover an odd sequence left by a killed producer; never reset the
        // counter on restart. All writers must hold the file's exclusive lock.
        let old = self.words[SEQ].load(SeqCst);
        let odd = if old & 1 == 0 {
            old.wrapping_add(1)
        } else {
            old.wrapping_add(2)
        };
        self.words[SEQ].store(odd, SeqCst);
        self.words[SESSION].store(input.session, SeqCst);
        self.words[HEARTBEAT].store(input.heartbeat, SeqCst);
        self.words[ONLINE].store(u32::from(input.online), SeqCst);
        self.words[SOURCE].store(input.source, SeqCst);
        self.words[AUX].store(
            u32::from(input.air) | (u32::from(input.buttons) << 8),
            SeqCst,
        );
        for (i, chunk) in input.slider.as_chunks::<4>().0.iter().enumerate() {
            self.words[SLIDER + i].store(u32::from_le_bytes(*chunk), SeqCst);
        }
        self.words[SEQ].store(odd.wrapping_add(1), SeqCst);
    }

    /// The LED writer is the Wine-side DLL (or the Linux `led-emit` probe);
    /// exactly one publisher at a time may call this. Publishes an odd seq,
    /// the payload, then the closing even seq, so readers never adopt a mix of
    /// two publications. `gen` counts publications for observability.
    fn publish_led(&self, led: &LedState) {
        let old = self.words[LED_SEQ].load(SeqCst);
        let odd = if old & 1 == 0 {
            old.wrapping_add(1)
        } else {
            old.wrapping_add(2) // Skip a stale odd left by a killed writer.
        };
        self.words[LED_SEQ].store(odd, SeqCst);
        let gen = self.words[LED_GEN].load(SeqCst).wrapping_add(1);
        self.words[LED_GEN].store(gen, SeqCst);
        for (i, chunk) in led.ground.as_chunks::<4>().0.iter().enumerate() {
            self.words[LED_GROUND + i].store(u32::from_le_bytes(*chunk), SeqCst);
        }
        self.words[LED_AIR_LEFT].store(
            u32::from_le_bytes([led.air_left[0], led.air_left[1], led.air_left[2], 0]),
            SeqCst,
        );
        self.words[LED_AIR_RIGHT].store(
            u32::from_le_bytes([led.air_right[0], led.air_right[1], led.air_right[2], 0]),
            SeqCst,
        );
        self.words[LED_SEQ].store(odd.wrapping_add(1), SeqCst);
    }

    fn led(&self) -> Option<LedState> {
        for _ in 0..3 {
            let before = self.words[LED_SEQ].load(SeqCst);
            if before & 1 != 0 {
                continue;
            }
            let mut led = LedState {
                ground: [0; LED_GROUND_ZONES * 3],
                air_left: [0; 3],
                air_right: [0; 3],
            };
            for (i, chunk) in led.ground.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                chunk.copy_from_slice(&self.words[LED_GROUND + i].load(SeqCst).to_le_bytes());
            }
            for (word, air) in [
                (LED_AIR_LEFT, &mut led.air_left),
                (LED_AIR_RIGHT, &mut led.air_right),
            ] {
                air.copy_from_slice(&self.words[word].load(SeqCst).to_le_bytes()[..3]);
            }
            if before == self.words[LED_SEQ].load(SeqCst) {
                return Some(led);
            }
        }
        None
    }

    fn snapshot(&self) -> Option<Snapshot> {
        for _ in 0..3 {
            let before = self.words[SEQ].load(SeqCst);
            if before & 1 != 0 {
                continue;
            }
            let aux = self.words[AUX].load(SeqCst);
            let mut input = Snapshot {
                session: self.words[SESSION].load(SeqCst),
                heartbeat: self.words[HEARTBEAT].load(SeqCst),
                online: self.words[ONLINE].load(SeqCst) != 0,
                source: self.words[SOURCE].load(SeqCst),
                air: aux as u8,
                buttons: (aux >> 8) as u8,
                slider: [0; 32],
            };
            for (i, chunk) in input.slider.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                chunk.copy_from_slice(&self.words[SLIDER + i].load(SeqCst).to_le_bytes());
            }
            if before == self.words[SEQ].load(SeqCst) {
                return Some(input);
            }
        }
        None
    }
}

pub struct ReadMapping {
    map: MmapRaw,
}

impl ReadMapping {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        ensure!(
            file.metadata()?.len() == REGION_SIZE as u64,
            "file must be exactly {REGION_SIZE} bytes"
        );
        // Raw mapping avoids creating an &[u8] alias of concurrently modified
        // storage. The producer never shrinks/replaces this trusted file.
        let map = MmapOptions::new()
            .len(REGION_SIZE)
            .map_raw_read_only(&file)?;
        let this = Self { map };
        this.region().validate()?;
        Ok(this)
    }

    fn region(&self) -> &Region {
        // SAFETY: size/alignment checked above; all bit patterns of every
        // AtomicU32 are valid. Only atomic loads occur through this RO mapping.
        unsafe { &*self.map.as_ptr().cast::<Region>() }
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        self.region().snapshot()
    }

    pub fn led(&self) -> Option<LedState> {
        self.region().led()
    }

    /// LED publication counter; 0 means no LED writer has ever published.
    /// Used for cheap change detection between coherent reads.
    pub fn led_generation(&self) -> u32 {
        self.region().words[LED_GEN].load(SeqCst)
    }
}

#[cfg(target_os = "linux")]
pub struct InputWriter {
    map: MmapRaw,
    _file: File, // Holds the single-writer lock until the mapping is dropped.
}

#[cfg(target_os = "linux")]
impl InputWriter {
    pub fn open(path: &Path) -> Result<Self> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("create/open {}", path.display()))?;
        ensure!(file.metadata()?.is_file(), "mapping must be a regular file");
        // SAFETY: geteuid has no preconditions.
        ensure!(
            file.metadata()?.uid() == unsafe { libc::geteuid() },
            "mapping must belong to the current user"
        );
        file.try_lock()
            .context("another producer holds the file lock")?;
        let len = file.metadata()?.len();
        if len == 0 {
            file.set_len(REGION_SIZE as u64)?;
        } else {
            ensure!(
                len == REGION_SIZE as u64,
                "refusing to resize an existing {len}-byte file"
            );
        }
        // Use raw pointers, never byte slices over the atomic storage. The
        // retained file lock excludes another writer, not concurrent readers.
        let map = MmapOptions::new().len(REGION_SIZE).map_raw(&file)?;
        let this = Self { map, _file: file };
        if this.region().words[0].load(SeqCst) == 0 {
            this.region().initialize();
        }
        if let Err(error) = this.region().validate() {
            if this.region().words[1].load(SeqCst) == 1 {
                bail!(
                    "found a protocol v1 experimental file; stop all users, remove {}, then restart",
                    path.display()
                );
            }
            return Err(error);
        }
        Ok(this)
    }

    fn region(&self) -> &Region {
        // SAFETY: the map is page aligned, 4096 bytes, and accessed atomically.
        unsafe { &*self.map.as_ptr().cast::<Region>() }
    }

    pub fn publish(&self, input: &Snapshot) {
        self.region().publish(input);
    }

    /// Read the LED region through the producer's read-write mapping. The
    /// bridge is an input writer but only a LED reader.
    pub fn led(&self) -> Option<LedState> {
        self.region().led()
    }
}

#[cfg(target_os = "linux")]
pub struct LedWriter {
    map: MmapRaw,
    _file: File, // Holds the LED-region range lock until the mapping is dropped.
}

#[cfg(target_os = "linux")]
impl LedWriter {
    /// Open (or create) the experimental region for LED publication. Unlike
    /// `InputWriter`, this takes a POSIX range lock over just the LED words:
    /// an input producer may legitimately hold the whole-file flock while a
    /// separate LED publisher runs. The Wine DLL does not participate in the
    /// range lock; run `led-emit` only while no game/DLL publishes LEDs.
    pub fn open(path: &Path) -> Result<Self> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("create/open {} for LED publication", path.display()))?;
        ensure!(file.metadata()?.is_file(), "mapping must be a regular file");
        // SAFETY: geteuid has no preconditions.
        ensure!(
            file.metadata()?.uid() == unsafe { libc::geteuid() },
            "mapping must belong to the current user"
        );
        let len = file.metadata()?.len();
        if len == 0 {
            file.set_len(REGION_SIZE as u64)?;
        } else {
            ensure!(
                len == REGION_SIZE as u64,
                "refusing to resize an existing {len}-byte file"
            );
        }
        let lock = libc::flock64 {
            l_type: libc::F_WRLCK as i16,
            l_whence: libc::SEEK_SET as i16,
            l_start: (LED_SEQ * 4) as i64,
            l_len: ((LED_AIR_RIGHT - LED_SEQ + 1) * 4) as i64,
            l_pid: 0,
        };
        // SAFETY: fcntl takes a valid fd and a properly initialized flock64.
        ensure!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } == 0,
            "another Linux LED publisher holds the LED region lock"
        );
        let map = MmapOptions::new().len(REGION_SIZE).map_raw(&file)?;
        let this = Self { map, _file: file };
        if this.region().words[0].load(SeqCst) == 0 {
            this.region().initialize();
        }
        if let Err(error) = this.region().validate() {
            if this.region().words[1].load(SeqCst) == 1 {
                bail!(
                    "found a protocol v1 experimental file; stop all users, remove {}, then restart",
                    path.display()
                );
            }
            return Err(error);
        }
        Ok(this)
    }

    fn region(&self) -> &Region {
        // SAFETY: the map is page aligned, 4096 bytes, and accessed atomically.
        unsafe { &*self.map.as_ptr().cast::<Region>() }
    }

    pub fn publish(&self, led: &LedState) {
        self.region().publish_led(led);
    }

    pub fn led(&self) -> Option<LedState> {
        self.region().led()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputState {
    Waiting,
    Offline,
    Live,
    Stale,
}

impl std::fmt::Display for InputState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Waiting => "waiting",
                Self::Offline => "offline",
                Self::Live => "live",
                Self::Stale => "stale",
            }
        )
    }
}

#[derive(Default)]
pub struct Liveness {
    observed: Option<(u32, u32, Instant)>,
    confirmed: bool,
}

impl Liveness {
    /// No comparison between Linux and Windows clocks. Require a heartbeat
    /// change after attaching/restarting before exposing any pressed inputs.
    pub fn filter(
        &mut self,
        input: &Snapshot,
        now: Instant,
        stale: Duration,
    ) -> (InputState, Snapshot) {
        match self.observed {
            Some((session, heartbeat, _))
                if session == input.session && heartbeat != input.heartbeat =>
            {
                self.observed = Some((input.session, input.heartbeat, now));
                self.confirmed = true;
            }
            Some((session, _, _)) if session == input.session => {}
            _ => {
                self.observed = Some((input.session, input.heartbeat, now));
                self.confirmed = false;
            }
        }
        let (_, _, last_change) = self.observed.unwrap();
        let state = if now.duration_since(last_change) >= stale {
            InputState::Stale
        } else if !input.online {
            InputState::Offline
        } else if !self.confirmed {
            InputState::Waiting
        } else {
            InputState::Live
        };
        let mut safe = input.clone();
        if state != InputState::Live {
            safe.slider = [0; 32];
            safe.air = 0;
            safe.buttons = 0;
            safe.online = false;
        }
        (state, safe)
    }
}

pub fn ensure_supported_platform() -> Result<()> {
    if !cfg!(any(target_os = "linux", target_os = "windows")) {
        bail!("only Linux and Windows/Wine are supported");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region() -> Region {
        let r = Region {
            words: std::array::from_fn(|_| AtomicU32::new(0)),
        };
        r.initialize();
        r
    }

    #[test]
    fn layout_and_roundtrip() {
        let r = region();
        r.validate().unwrap();
        let sample = Snapshot::synthetic(123, u32::MAX);
        r.publish(&sample);
        assert_eq!(r.snapshot(), Some(sample));
        assert_eq!(std::mem::offset_of!(Region, words), 0);
        assert_eq!(SEQ * 4, 64);
        assert_eq!(SLIDER * 4, 96);
    }

    #[test]
    fn parser_uses_exact_offsets_and_keeps_aux() {
        let raw: Vec<u8> = (0..45).collect();
        let s = parse_report(&raw).unwrap();
        assert_eq!((s.air, s.buttons), (0, 1));
        assert_eq!(s.slider, std::array::from_fn(|i| (i + 2) as u8));
        for len in [0, 2, 34, 44, 46, 64] {
            assert!(parse_report(&vec![0; len]).is_err());
        }
    }

    #[test]
    fn game_permutation_matches_reference() {
        let raw = std::array::from_fn(|i| i as u8);
        assert_eq!(
            game_order(&raw),
            [
                30, 31, 28, 29, 26, 27, 24, 25, 22, 23, 20, 21, 18, 19, 16, 17, 14, 15, 12, 13, 10,
                11, 8, 9, 6, 7, 4, 5, 2, 3, 0, 1
            ]
        );
    }

    fn sample_led() -> LedState {
        let mut led = LedState::default();
        for zone in 0..LED_GROUND_ZONES {
            led.ground[zone * 3..zone * 3 + 3].copy_from_slice(&[
                (zone * 3) as u8,
                (zone * 3 + 1) as u8,
                (zone * 3 + 2) as u8,
            ]);
        }
        led.air_left = [1, 2, 3];
        led.air_right = [4, 5, 6];
        led
    }

    #[test]
    fn led_layout_and_roundtrip() {
        let r = region();
        let led = sample_led();
        r.publish_led(&led);
        assert_eq!(r.led(), Some(led.clone()));
        assert_eq!(r.words[LED_GEN].load(SeqCst), 1);
        r.publish_led(&led);
        assert_eq!(r.led(), Some(led));
        assert_eq!(r.words[LED_GEN].load(SeqCst), 2);
        assert_eq!(r.words[LED_SEQ].load(SeqCst) & 1, 0);
        // Pin the documented v2 byte offsets.
        assert_eq!(LED_SEQ * 4, 128);
        assert_eq!(LED_GEN * 4, 132);
        assert_eq!(LED_GROUND * 4, 136);
        assert_eq!(LED_AIR_LEFT * 4, 232);
        assert_eq!(LED_AIR_RIGHT * 4, 236);
    }

    #[test]
    fn led_reader_rejects_busy_region_and_survives_wrap() {
        let r = region();
        r.words[LED_SEQ].store(u32::MAX, SeqCst);
        assert_eq!(r.led(), None);
        let led = sample_led();
        r.publish_led(&led); // Recovers the stale odd seq by skipping past it.
        assert_eq!(r.words[LED_SEQ].load(SeqCst), 2);
        assert_eq!(r.led(), Some(led));
    }

    #[test]
    fn led_payload_never_tears() {
        let r = std::sync::Arc::new(region());
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        r.publish_led(&LedState::default());
        let worker = {
            let r = r.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let mut led = LedState::default();
                for n in 1..50_000_u32 {
                    led.ground.fill(n as u8);
                    led.air_left = [n as u8, n as u8, n as u8];
                    r.publish_led(&led);
                }
                done.store(true, SeqCst);
            })
        };
        while !done.load(SeqCst) {
            if let Some(led) = r.led() {
                let fill = led.ground[0];
                assert!(led.ground.iter().all(|&b| b == fill));
                assert_eq!(led.air_left, [fill, fill, fill]);
            }
        }
        worker.join().unwrap();
        assert_eq!(r.led().unwrap().ground[0], 49_999_u32 as u8);
    }

    #[test]
    fn led_packets_match_rpunithm_rgb_layout() {
        let mut led = LedState::default();
        for zone in 0..31 {
            led.ground[zone * 3..zone * 3 + 3].copy_from_slice(&[
                (zone * 3) as u8,
                (zone * 3 + 1) as u8,
                (zone * 3 + 2) as u8,
            ]);
        }
        led.ground[31 * 3..].copy_from_slice(&[0xEE; 3]); // Zone 31 has no device LED.
        led.air_left = [0x11, 0x22, 0x33];
        led.air_right = [0xAA, 0xBB, 0xCC];
        let packets = led_packets(&led);
        assert_eq!(packets[0][0], 0);
        assert_eq!(packets[1][0], 1);
        // Slider RGB stays unchanged, including both sides of the packet boundary.
        assert_eq!(&packets[0][1..4], &[0, 1, 2]);
        // Zones 0..19 fill packet 0 slots 0..19.
        assert_eq!(&packets[0][58..61], &[57, 58, 59]);
        // Zones 20..30 fill packet 1 slots 0..10.
        assert_eq!(&packets[1][1..4], &[60, 61, 62]);
        assert_eq!(&packets[1][31..34], &[90, 91, 92]);
        // The ToF device has no air LEDs. Even nonzero compatibility SHM
        // colors must not populate any non-slider output bytes.
        assert_eq!(&packets[1][34..], &[0; 27]);
        // The unlit zone 31 never reaches the wire.
        assert!(!packets[0]
            .iter()
            .chain(packets[1].iter())
            .any(|&b| b == 0xEE));
        assert_eq!(packets[0].len(), LED_PACKET_LEN);
        assert_eq!(packets[1].len(), LED_PACKET_LEN);
    }

    #[test]
    fn led_slider_primary_colors_are_not_rotated() {
        // Hardware feedback for the old GBR path was R->blue, G->red, B->green.
        // Prevent reintroducing that rotation for either slider packet.
        for color in [[0x40, 0, 0], [0, 0x40, 0], [0, 0, 0x40]] {
            for (zone, packet, offset) in [(7, 0, 22), (27, 1, 22)] {
                let mut led = LedState::default();
                led.ground[zone * 3..zone * 3 + 3].copy_from_slice(&color);
                let packets = led_packets(&led);
                assert_eq!(&packets[packet][offset..offset + 3], &color);
                for (packet_index, report) in packets.iter().enumerate() {
                    for (index, &byte) in report.iter().enumerate().skip(1) {
                        if packet_index != packet || !(offset..offset + 3).contains(&index) {
                            assert_eq!(byte, 0);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn bounded_read_recovers_from_killed_writer_and_sequence_wrap() {
        let r = region();
        r.words[SEQ].store(u32::MAX, SeqCst);
        assert!(r.snapshot().is_none());
        let sample = Snapshot::synthetic(2, 9);
        r.publish(&sample);
        assert_eq!(r.snapshot(), Some(sample));
        r.words[SEQ].store(u32::MAX - 1, SeqCst);
        r.publish(&Snapshot::default());
        assert_eq!(r.words[SEQ].load(SeqCst), 0);
        assert_eq!(r.snapshot(), Some(Snapshot::default()));
    }

    #[test]
    fn no_pressed_input_until_heartbeat_moves_and_zero_after_stall() {
        let mut live = Liveness::default();
        let now = Instant::now();
        let ttl = Duration::from_millis(100);
        let first = Snapshot::synthetic(1, 1);
        let (state, safe) = live.filter(&first, now, ttl);
        assert_eq!(state, InputState::Waiting);
        assert_eq!(safe.slider, [0; 32]);
        let second = Snapshot::synthetic(1, 2);
        assert_eq!(
            live.filter(&second, now + Duration::from_millis(10), ttl).0,
            InputState::Live
        );
        let (state, safe) = live.filter(&second, now + Duration::from_millis(110), ttl);
        assert_eq!(state, InputState::Stale);
        assert_eq!(safe.slider, [0; 32]);
        let restart = Snapshot::synthetic(2, 1);
        assert_eq!(
            live.filter(&restart, now + Duration::from_millis(120), ttl)
                .0,
            InputState::Waiting
        );
        assert_eq!(
            live.filter(
                &Snapshot::synthetic(2, 2),
                now + Duration::from_millis(130),
                ttl
            )
            .0,
            InputState::Live
        );
        assert_eq!(
            live.filter(&Snapshot::default(), now + Duration::from_millis(140), ttl)
                .0,
            InputState::Offline
        );
    }

    #[test]
    fn concurrent_payload_never_tears() {
        let r = std::sync::Arc::new(region());
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        r.publish(&Snapshot::synthetic(7, 0));
        let worker = {
            let r = r.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                for n in 1..50_000 {
                    r.publish(&Snapshot::synthetic(7, n));
                }
                done.store(true, SeqCst);
            })
        };
        while !done.load(SeqCst) {
            if let Some(s) = r.snapshot() {
                s.verify_synthetic().unwrap();
            }
        }
        worker.join().unwrap();
        assert_eq!(r.snapshot().unwrap().heartbeat, 49_999);
    }
}
