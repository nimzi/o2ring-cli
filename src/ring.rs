// SPDX-License-Identifier: AGPL-3.0-or-later
//! BLE session with an O2Ring S over the vendor OxyII service, via BlueZ (`bluer`).
//!
//! Only read-only commands can be sent: the allowlist below has no settings, clock,
//! delete, reset or firmware opcodes, so a bug here can't change or erase the ring.

use std::{pin::Pin, time::Duration};

use anyhow::{anyhow, bail, ensure, Context, Result};
use bluer::{
    gatt::{
        remote::{Characteristic, CharacteristicWriteRequest},
        WriteOp,
    },
    Adapter, AdapterEvent, Address, Device, Uuid,
};
use futures::{pin_mut, Stream, StreamExt};
use tokio::time::{sleep, timeout, Instant};

use crate::crypto;
use crate::frame::{encode, Decoder};

pub const SERVICE: Uuid = Uuid::from_u128(0xe8fb0001_a14b_98f9_831b_4e2941d01248);
pub const WRITE_CHAR: Uuid = Uuid::from_u128(0xe8fb0002_a14b_98f9_831b_4e2941d01248);
pub const NOTIFY_CHAR: Uuid = Uuid::from_u128(0xe8fb0003_a14b_98f9_831b_4e2941d01248);

pub mod cmd {
    pub const GET_CONFIG: u8 = 0x00;
    pub const RT_DATA: u8 = 0x04;
    pub const AUTO_RT_SWITCH: u8 = 0x10;
    pub const GET_INFO: u8 = 0xE1;
    pub const GET_BATTERY: u8 = 0xE4;
    pub const GET_FILE_LIST: u8 = 0xF1;
    pub const READ_FILE_START: u8 = 0xF2;
    pub const READ_FILE_DATA: u8 = 0xF3;
    pub const READ_FILE_END: u8 = 0xF4;
    pub const AUTH: u8 = 0xFF;
    // Write commands: only sendable on a connection opened in `Mode::Write`.
    pub const SET_CONFIG: u8 = 0x01;
    pub const SET_UTC_TIME: u8 = 0xC0;
    pub const SET_TIME: u8 = 0xEC;
}

/// What a connection is allowed to send. Everything opens `ReadOnly` (including the
/// unattended `sync`); only `set` and `set-clock` open `Write`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    ReadOnly,
    Write,
}

/// Opcodes any connection may send. Nothing here writes to the ring's storage or settings
/// (AUTO_RT_SWITCH only stops live-data pushes for this connection).
const ALLOWED: &[u8] = &[
    cmd::AUTH,
    cmd::AUTO_RT_SWITCH,
    cmd::GET_CONFIG,
    cmd::RT_DATA,
    cmd::GET_INFO,
    cmd::GET_BATTERY,
    cmd::GET_FILE_LIST,
    cmd::READ_FILE_START,
    cmd::READ_FILE_DATA,
    cmd::READ_FILE_END,
];

/// Additional opcodes allowed in `Mode::Write`: settings and clock only. Delete, reset,
/// factory and firmware opcodes are in neither list, so they can never be sent.
const ALLOWED_WRITE: &[u8] = &[cmd::SET_CONFIG, cmd::SET_UTC_TIME, cmd::SET_TIME];

pub fn allowed(mode: Mode, op: u8) -> bool {
    ALLOWED.contains(&op) || (mode == Mode::Write && ALLOWED_WRITE.contains(&op))
}

/// SET_CONFIG setting types (vendor `OxyIIBleCmd.ConfigType`).
pub mod setting {
    pub const SPO2_SWITCH: u8 = 1;
    pub const SPO2_LOW: u8 = 2;
    pub const HR_SWITCH: u8 = 3;
    pub const HR_LOW: u8 = 4;
    pub const HR_HIGH: u8 = 5;
    pub const MOTOR: u8 = 6;
    pub const DISPLAY_MODE: u8 = 8;
    pub const BRIGHTNESS: u8 = 9;
    pub const INTERVAL: u8 = 10;
}

pub fn set_config_payload(typ: u8, value: u16) -> [u8; 8] {
    let v = value.to_le_bytes();
    [typ, 0, 0, 0, v[0], v[1], 0, 0]
}

/// SET_UTC_TIME (0xC0) payload: local year u16, month, day, h, m, s, then the UTC offset in
/// tenths of an hour (truncated, as the vendor app does). SET_TIME (0xEC) is the first 7 bytes.
pub fn time_payload(now: &chrono::DateTime<chrono::Local>) -> [u8; 8] {
    use chrono::{Datelike, Offset, Timelike};
    let y = (now.year() as u16).to_le_bytes();
    let tz = (now.offset().fix().local_minus_utc() / 60 / 6) as i8;
    [
        y[0],
        y[1],
        now.month() as u8,
        now.day() as u8,
        now.hour() as u8,
        now.minute() as u8,
        now.second() as u8,
        tz as u8,
    ]
}

/// Replies whose payload is AES-encrypted in an encrypted session (vendor `OxyIIBleInterface`).
const ENCRYPTED_REPLIES: &[u8] = &[
    0xE0, 0xE1, 0xE4, 0xF1, 0x06, 0xF2, 0xF3, 0x00, 0x02, 0x11, 0x03, 0x12, 0x04, 0x05, 0x13, 0x14,
];

/// MD5("lepucloud"), used to obfuscate the AUTH exchange.
const LEPU_KEY: [u8; 16] = [
    0xc2, 0xa7, 0xcf, 0x50, 0xda, 0xfe, 0xd8, 0x85, 0xa8, 0xf8, 0xf7, 0xea, 0xc4, 0x43, 0x35, 0xf3,
];

/// BLE writes are split into chunks this size (safe with the default ATT MTU).
const WRITE_CHUNK: usize = 20;

#[derive(Debug)]
pub enum ReqError {
    Timeout(u8),
    Rejected(u8, u8),
    Disconnected,
    Other(anyhow::Error),
}

impl std::fmt::Display for ReqError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout(c) => write!(f, "timeout waiting for reply to 0x{c:02x}"),
            Self::Rejected(c, flag) => {
                let why = match flag {
                    0x02 => "CRC error",
                    0xE0 => "file not found",
                    0xE1 => "file read failed",
                    0xF1 => "illegal parameter",
                    0xF2 => "permission denied",
                    0xFB => "busy",
                    0xFD => "unsupported",
                    _ => "error",
                };
                write!(f, "ring rejected 0x{c:02x}: {why} (0x{flag:02x})")
            }
            Self::Disconnected => write!(f, "ring disconnected"),
            Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ReqError {}

impl From<bluer::Error> for ReqError {
    fn from(e: bluer::Error) -> Self {
        Self::Other(e.into())
    }
}

pub struct Info {
    pub firmware: String,
    pub serial: String,
    pub clock: String,
}

pub struct Battery {
    pub percent: u8,
    pub state: &'static str,
}

fn battery_state(b: u8) -> &'static str {
    match b {
        0 => "normal",
        1 => "charging",
        2 => "full",
        3 => "low",
        _ => "unknown",
    }
}

/// Device settings from GET_CONFIG (0x00).
#[derive(Debug)]
pub struct Config {
    pub spo2_vibrate: bool,
    pub spo2_sound: bool,
    pub pulse_vibrate: bool,
    pub pulse_sound: bool,
    pub spo2_threshold: u8,
    pub pulse_low: u8,
    pub pulse_high: u8,
    pub motor: u8,
    pub buzzer: u8,
    pub display_mode: u8,
    pub brightness: u8,
    pub interval_s: u8,
    /// Hours from UTC (stored in tenths), when the reply carries it.
    pub utc_offset_h: Option<f32>,
    pub raw: Vec<u8>,
}

pub fn parse_config(d: &[u8]) -> Result<Config> {
    ensure!(d.len() >= 9, "GET_CONFIG reply too short ({} bytes)", d.len());
    Ok(Config {
        spo2_vibrate: d[0] & 0x01 != 0,
        spo2_sound: d[0] & 0x02 != 0,
        pulse_vibrate: d[0] & 0x10 != 0,
        pulse_sound: d[0] & 0x20 != 0,
        spo2_threshold: d[1],
        pulse_low: d[2],
        pulse_high: d[3],
        motor: d[4],
        buzzer: d[5],
        display_mode: d[6],
        brightness: d[7],
        interval_s: d[8],
        utc_offset_h: d.get(9).map(|&b| b as i8 as f32 / 10.0),
        raw: d.to_vec(),
    })
}

/// One live reading from RT_DATA (0x04). Values are `None` when the ring reports them invalid.
#[derive(Debug)]
pub struct Live {
    pub status: &'static str,
    pub sensor: &'static str,
    pub spo2: Option<u8>,
    pub pulse: Option<u16>,
    pub pi: Option<f32>,
    pub motion: Option<u8>,
    pub battery: u8,
    pub battery_state: &'static str,
}

pub fn parse_live(d: &[u8]) -> Result<Live> {
    ensure!(d.len() >= 14, "RT_DATA reply too short ({} bytes)", d.len());
    let spo2 = d[6];
    let pr = u16::from_le_bytes([d[8], d[9]]);
    let valid = ![0, 127, 255].contains(&spo2) && ![0, 255, 511, 65535].contains(&pr);
    Ok(Live {
        status: ["idle", "preparing", "measuring", "finished"].get(d[4] as usize).copied().unwrap_or("unknown"),
        sensor: ["no finger", "ok", "probe off", "fault"].get(d[5] as usize).copied().unwrap_or("unknown"),
        spo2: valid.then_some(spo2),
        pulse: valid.then_some(pr),
        pi: (valid && d[7] != 0 && d[7] != 255).then(|| d[7] as f32 / 10.0),
        // Motion only means something while a finger is on and the ring is recording.
        motion: (d[4] == 2 && d[5] == 1 && d[11] != 255).then_some(d[11]),
        battery: d[13],
        battery_state: battery_state(d[12]),
    })
}

pub fn is_ring_name(name: &str) -> bool {
    name.starts_with("S8-AW") || name.starts_with("O2Ring")
}

/// Scan until the ring is seen (by address, or by advertised name), or time out.
pub async fn find(adapter: &Adapter, addr: Option<Address>, wait: Duration) -> Result<Device> {
    let events = adapter.discover_devices().await?;
    pin_mut!(events);
    let deadline = sleep(wait);
    pin_mut!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => bail!(
                "ring not found within {}s: tap its screen (or put it on) to wake it, \
                 and make sure ViHealth or a browser isn't connected to it",
                wait.as_secs()
            ),
            ev = events.next() => match ev {
                Some(AdapterEvent::DeviceAdded(a)) => {
                    let dev = adapter.device(a)?;
                    let hit = match addr {
                        Some(want) => a == want,
                        None => dev.name().await?.is_some_and(|n| is_ring_name(&n)),
                    };
                    if hit {
                        return Ok(dev);
                    }
                }
                Some(_) => {}
                None => bail!("Bluetooth discovery ended unexpectedly"),
            },
        }
    }
}

pub struct Ring {
    device: Device,
    write: Characteristic,
    notify: Pin<Box<dyn Stream<Item = Vec<u8>> + Send>>,
    decoder: Decoder,
    seq: u8,
    seq_echoed: bool,
    /// Session key, when AUTH returned one (newer firmware); `None` means plaintext.
    aes_key: Option<[u8; 16]>,
    mode: Mode,
}

impl Ring {
    pub async fn connect(device: Device, mode: Mode) -> Result<Self> {
        if !device.is_connected().await? {
            let mut last = None;
            for _ in 0..3 {
                match timeout(Duration::from_secs(15), device.connect()).await {
                    Ok(Ok(())) => {
                        last = None;
                        break;
                    }
                    Ok(Err(e)) => last = Some(anyhow!(e)),
                    Err(_) => last = Some(anyhow!("connection attempt timed out")),
                }
                sleep(Duration::from_secs(1)).await;
            }
            if let Some(e) = last {
                return Err(e.context(
                    "couldn't connect: the ring may have gone to sleep (tap its screen) \
                     or another app holds the connection",
                ));
            }
        }
        for _ in 0..100 {
            if device.is_services_resolved().await? {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        let (mut write, mut notify) = (None, None);
        for s in device.services().await? {
            if s.uuid().await? != SERVICE {
                continue;
            }
            for c in s.characteristics().await? {
                match c.uuid().await? {
                    u if u == WRITE_CHAR => write = Some(c),
                    u if u == NOTIFY_CHAR => notify = Some(c),
                    _ => {}
                }
            }
        }
        let write = write.context("OxyII write characteristic not found (not an O2Ring S?)")?;
        let notify = notify.context("OxyII notify characteristic not found")?;
        let stream = notify.notify().await?;

        let mut ring = Ring {
            device,
            write,
            notify: Box::pin(stream),
            decoder: Decoder::default(),
            seq: 1,
            seq_echoed: false,
            aes_key: None,
            mode,
        };
        ring.handshake().await?;
        Ok(ring)
    }

    pub fn encrypted(&self) -> bool {
        self.aes_key.is_some()
    }

    /// Same opening as the vendor SDK: AUTH, then turn off automatic live-data pushes.
    /// Older O2Ring S firmware (1.0.x) doesn't answer AUTH and stays plaintext; newer firmware
    /// (seen on 1.13.1.0) answers with an AES key and the session is encrypted from then on.
    async fn handshake(&mut self) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as u32;
        let mut p = [0u8; 16];
        for i in 0..8 {
            p[i] = LEPU_KEY[i * 2];
        }
        p[8..12].copy_from_slice(b"0000");
        p[12..16].copy_from_slice(&now.to_le_bytes());
        for i in 0..16 {
            p[i] ^= LEPU_KEY[i];
        }
        match self.request(cmd::AUTH, &p, Duration::from_secs(1)).await {
            Ok(reply) => {
                // XOR-decode: [type=1, len=16, ?, ?, key16]
                let d: Vec<u8> = reply.iter().enumerate().map(|(i, b)| b ^ LEPU_KEY[i % 16]).collect();
                if d.len() >= 20 && d[0] == 1 && d[1] == 16 {
                    let mut key = [0u8; 16];
                    key.copy_from_slice(&d[4..20]);
                    self.aes_key = Some(key);
                }
            }
            Err(ReqError::Timeout(_)) => {} // older firmware: no reply, stay plaintext
            Err(e) => return Err(e.into()),
        }
        if let Err(e) = self.request(cmd::AUTO_RT_SWITCH, &[0], Duration::from_secs(3)).await {
            eprintln!("warning: couldn't turn off live-data pushes: {e}");
        }
        Ok(())
    }

    pub async fn request(&mut self, op: u8, payload: &[u8], wait: Duration) -> Result<Vec<u8>, ReqError> {
        if !allowed(self.mode, op) {
            return Err(ReqError::Other(anyhow!(
                "refusing to send opcode 0x{op:02x} on a {:?} connection",
                self.mode
            )));
        }
        let seq = self.seq;
        self.seq = if self.seq >= 254 { 1 } else { self.seq + 1 };
        // AUTH is always plaintext; empty payloads are never encrypted.
        let body = match &self.aes_key {
            Some(k) if !payload.is_empty() && op != cmd::AUTH => crypto::encrypt(k, payload),
            _ => payload.to_vec(),
        };
        let frame = encode(op, &body, seq);
        let req = CharacteristicWriteRequest { op_type: WriteOp::Command, ..Default::default() };
        for chunk in frame.chunks(WRITE_CHUNK) {
            self.write.write_ext(chunk, &req).await?;
        }
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let data = match timeout(left, self.notify.next()).await {
                Err(_) => {
                    // Drop any half-received frame so it can't stall the next reply.
                    self.decoder.reset();
                    return Err(ReqError::Timeout(op));
                }
                Ok(None) => return Err(ReqError::Disconnected),
                Ok(Some(d)) => d,
            };
            for f in self.decoder.push(&data) {
                // The O2Ring S echoes seq; once seen, use it to drop late replies.
                if f.cmd == op && (f.seq == seq || !self.seq_echoed) {
                    if f.seq == seq {
                        self.seq_echoed = true;
                    }
                    if f.flag != 1 {
                        return Err(ReqError::Rejected(op, f.flag));
                    }
                    return match &self.aes_key {
                        Some(k)
                            if !f.payload.is_empty()
                                && f.payload.len().is_multiple_of(16)
                                && ENCRYPTED_REPLIES.contains(&op) =>
                        {
                            crypto::decrypt(k, &f.payload)
                                .map_err(|e| ReqError::Other(e.context(format!("decrypting reply to 0x{op:02x}"))))
                        }
                        _ => Ok(f.payload),
                    };
                }
            }
        }
    }

    pub async fn info(&mut self) -> Result<Info> {
        let d = self.request(cmd::GET_INFO, &[], Duration::from_secs(3)).await?;
        ensure!(d.len() >= 38, "GET_INFO reply too short ({} bytes)", d.len());
        let firmware = format!("{}.{}.{}.{}", d[4], d[3], d[2], d[1]);
        let year = u16::from_le_bytes([d[24], d[25]]);
        let clock = format!(
            "{year:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            d[26], d[27], d[28], d[29], d[30]
        );
        let sn_len = d[37] as usize;
        let serial = d
            .get(38..38 + sn_len)
            .map(|s| String::from_utf8_lossy(s).trim_end_matches('\0').to_string())
            .unwrap_or_default();
        Ok(Info { firmware, serial, clock })
    }

    pub async fn battery(&mut self) -> Result<Battery> {
        let d = self.request(cmd::GET_BATTERY, &[], Duration::from_secs(3)).await?;
        ensure!(d.len() >= 2, "GET_BATTERY reply too short");
        Ok(Battery { percent: d[1], state: battery_state(d[0]) })
    }

    pub async fn config(&mut self) -> Result<Config> {
        let d = self.request(cmd::GET_CONFIG, &[], Duration::from_secs(3)).await?;
        parse_config(&d)
    }

    pub async fn live(&mut self) -> Result<Live> {
        let d = self.request(cmd::RT_DATA, &[], Duration::from_millis(2500)).await?;
        parse_live(&d)
    }

    /// Recording names on the ring (`yyyyMMddHHmmss`), oldest first as the ring lists them.
    pub async fn list_files(&mut self) -> Result<Vec<String>> {
        let d = self.request(cmd::GET_FILE_LIST, &[], Duration::from_secs(5)).await?;
        let Some(&n) = d.first() else { return Ok(vec![]) };
        Ok((0..n as usize)
            .filter_map(|i| d.get(1 + 16 * i..1 + 16 * (i + 1)))
            .map(|s| String::from_utf8_lossy(s).trim_end_matches('\0').trim().to_string())
            .filter(|s| !s.is_empty())
            .collect())
    }

    pub async fn read_file(&mut self, name: &str, mut progress: impl FnMut(usize, usize)) -> Result<Vec<u8>> {
        let res = self.read_file_inner(name, &mut progress).await;
        // Always close the transfer, even after an error.
        let _ = self.request(cmd::READ_FILE_END, &[], Duration::from_secs(3)).await;
        res
    }

    async fn read_file_inner(&mut self, name: &str, progress: &mut impl FnMut(usize, usize)) -> Result<Vec<u8>> {
        let mut p = [0u8; 20];
        let nb = name.as_bytes();
        let k = nb.len().min(16);
        p[..k].copy_from_slice(&nb[..k]);
        let start = self.request(cmd::READ_FILE_START, &p, Duration::from_secs(5)).await?;
        ensure!(start.len() >= 4, "READ_FILE_START reply too short");
        let size = u32::from_le_bytes([start[0], start[1], start[2], start[3]]) as usize;
        ensure!(size > 0, "the ring reported {name} as empty");
        ensure!(size <= crate::recording::MAX_FILE, "implausible file size {size} for {name}");

        let mut out = Vec::with_capacity(size);
        while out.len() < size {
            let offset = (out.len() as u32).to_le_bytes();
            let mut chunk = None;
            for attempt in 0..3 {
                // Retrying the same offset is safe: late replies are dropped by seq.
                match self.request(cmd::READ_FILE_DATA, &offset, Duration::from_secs(6)).await {
                    Ok(c) => {
                        chunk = Some(c);
                        break;
                    }
                    Err(ReqError::Timeout(_)) if attempt < 2 => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            let chunk = chunk.expect("loop either sets chunk or returns");
            ensure!(!chunk.is_empty(), "empty file chunk at offset {}", out.len());
            let n = chunk.len().min(size - out.len());
            out.extend_from_slice(&chunk[..n]);
            progress(out.len(), size);
        }
        Ok(out)
    }

    /// Write one setting (SET_CONFIG). Callers validate the value and read back afterwards.
    pub async fn set_config(&mut self, typ: u8, value: u16) -> Result<()> {
        self.request(cmd::SET_CONFIG, &set_config_payload(typ, value), Duration::from_secs(3))
            .await?;
        Ok(())
    }

    /// Set the ring's clock to the computer's local time. Tries SET_UTC_TIME (with time zone,
    /// as ViHealth does) and falls back to SET_TIME (as O2 Insight does) if the ring rejects it.
    pub async fn set_clock(&mut self, now: &chrono::DateTime<chrono::Local>) -> Result<&'static str> {
        let p = time_payload(now);
        match self.request(cmd::SET_UTC_TIME, &p, Duration::from_secs(3)).await {
            Ok(_) => Ok("SET_UTC_TIME (0xC0)"),
            Err(ReqError::Rejected(..)) => {
                self.request(cmd::SET_TIME, &p[..7], Duration::from_secs(3)).await?;
                Ok("SET_TIME (0xEC)")
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn disconnect(self) {
        let _ = self.device.disconnect().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config() {
        let c = parse_config(&[0x01, 88, 50, 120, 60, 0, 0, 1, 1, 0xB0]).unwrap();
        assert!(c.spo2_vibrate && !c.spo2_sound && !c.pulse_vibrate);
        assert_eq!((c.spo2_threshold, c.pulse_low, c.pulse_high, c.interval_s), (88, 50, 120, 1));
        assert_eq!(c.utc_offset_h, Some(-8.0));
    }

    #[test]
    fn parses_live_and_invalid_values() {
        let mut d = [0u8; 20];
        d[4] = 2; // measuring
        d[5] = 1; // sensor ok
        d[6] = 97;
        d[7] = 35;
        d[8..10].copy_from_slice(&64u16.to_le_bytes());
        d[11] = 3;
        d[13] = 90;
        let s = parse_live(&d).unwrap();
        assert_eq!((s.spo2, s.pulse, s.pi, s.motion), (Some(97), Some(64), Some(3.5), Some(3)));
        d[6] = 127; // no reading
        d[5] = 0;
        let s = parse_live(&d).unwrap();
        assert_eq!((s.spo2, s.pulse, s.motion, s.sensor), (None, None, None, "no finger"));
    }
}

#[cfg(test)]
mod write_tests {
    use super::*;

    #[test]
    fn read_only_connections_cannot_write() {
        for op in [cmd::SET_CONFIG, cmd::SET_UTC_TIME, cmd::SET_TIME] {
            assert!(!allowed(Mode::ReadOnly, op));
            assert!(allowed(Mode::Write, op));
        }
        // Destructive opcodes are never allowed, in any mode.
        for op in [0xE2, 0xE3, 0xEE, 0xE5, 0xE6, 0xE7, 0xFA, 0xEA, 0xEB, 0xF5, 0xF6, 0xF7, 0xF8] {
            assert!(!allowed(Mode::ReadOnly, op) && !allowed(Mode::Write, op), "0x{op:02x}");
        }
    }

    #[test]
    fn payloads() {
        assert_eq!(set_config_payload(2, 89), [2, 0, 0, 0, 89, 0, 0, 0]);
        use chrono::TimeZone;
        let t = chrono::Local.with_ymd_and_hms(2026, 11, 9, 10, 5, 7).unwrap();
        let p = time_payload(&t);
        assert_eq!(&p[..7], &[0xEA, 0x07, 11, 9, 10, 5, 7]);
    }
}
