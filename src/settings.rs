// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writable ring settings: names, allowed values, and read-back checking.
//!
//! Allowed values follow O2Ring Web's settings screen for the O2Ring S, so the ring is only
//! ever sent values its own apps would send.

use anyhow::{bail, ensure, Result};
use clap::ValueEnum;

use crate::ring::{setting, Config};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Setting {
    /// Vibrate when SpO₂ falls below the threshold: on | off
    Spo2Reminder,
    /// SpO₂ reminder threshold: 80–95 (%)
    Spo2Threshold,
    /// Vibrate when pulse leaves the low/high range: on | off
    PulseReminder,
    /// Low pulse threshold: 30–70 bpm, in steps of 5
    PulseLow,
    /// High pulse threshold: 70–200 bpm, in steps of 5
    PulseHigh,
    /// Vibration strength: 20 | 40 | 60 | 80 | 100
    Vibration,
    /// Screen mode: standard | always-on
    Display,
    /// Screen brightness: 0–2 (or low | medium | high)
    Brightness,
    /// Storage interval: 1 | 4 (seconds)
    Interval,
}

impl Setting {
    pub fn name(self) -> &'static str {
        match self {
            Self::Spo2Reminder => "spo2-reminder",
            Self::Spo2Threshold => "spo2-threshold",
            Self::PulseReminder => "pulse-reminder",
            Self::PulseLow => "pulse-low",
            Self::PulseHigh => "pulse-high",
            Self::Vibration => "vibration",
            Self::Display => "display",
            Self::Brightness => "brightness",
            Self::Interval => "interval",
        }
    }

    /// SET_CONFIG type.
    pub fn config_type(self) -> u8 {
        match self {
            Self::Spo2Reminder => setting::SPO2_SWITCH,
            Self::Spo2Threshold => setting::SPO2_LOW,
            Self::PulseReminder => setting::HR_SWITCH,
            Self::PulseLow => setting::HR_LOW,
            Self::PulseHigh => setting::HR_HIGH,
            Self::Vibration => setting::MOTOR,
            Self::Display => setting::DISPLAY_MODE,
            Self::Brightness => setting::BRIGHTNESS,
            Self::Interval => setting::INTERVAL,
        }
    }

    /// Where this setting lives in the GET_CONFIG reply: (byte index, bit mask).
    fn slot(self) -> (usize, u8) {
        match self {
            Self::Spo2Reminder => (0, 0x01),
            Self::PulseReminder => (0, 0x10),
            Self::Spo2Threshold => (1, 0xFF),
            Self::PulseLow => (2, 0xFF),
            Self::PulseHigh => (3, 0xFF),
            Self::Vibration => (4, 0xFF),
            Self::Display => (6, 0xFF),
            Self::Brightness => (7, 0xFF),
            Self::Interval => (8, 0xFF),
        }
    }

    /// Parse and range-check a value typed by the user. Returns the value as GET_CONFIG
    /// reports it (toggles as 1/0). Nothing is sent if this fails.
    pub fn parse(self, s: &str) -> Result<u8> {
        let s = s.trim().to_ascii_lowercase();
        let num = |lo: u8, hi: u8, step: u8| -> Result<u8> {
            let v: u8 = s.parse().ok().filter(|v| (lo..=hi).contains(v) && (v - lo).is_multiple_of(step)).ok_or_else(|| {
                if step == 1 {
                    anyhow::anyhow!("{} must be {lo}–{hi}", self.name())
                } else {
                    anyhow::anyhow!("{} must be {lo}–{hi} in steps of {step}", self.name())
                }
            })?;
            Ok(v)
        };
        Ok(match self {
            Self::Spo2Reminder | Self::PulseReminder => match s.as_str() {
                "on" => 1,
                "off" => 0,
                _ => bail!("{} must be on or off", self.name()),
            },
            Self::Spo2Threshold => num(80, 95, 1)?,
            Self::PulseLow => num(30, 70, 5)?,
            Self::PulseHigh => num(70, 200, 5)?,
            Self::Vibration => match s.as_str() {
                "20" => 20,
                "40" => 40,
                "60" => 60,
                "80" => 80,
                "100" => 100,
                _ => bail!("vibration must be 20, 40, 60, 80 or 100"),
            },
            Self::Display => match s.as_str() {
                "standard" => 0,
                "always-on" => 2,
                _ => bail!("display must be standard or always-on"),
            },
            Self::Brightness => match s.as_str() {
                "0" | "low" => 0,
                "1" | "medium" => 1,
                "2" | "high" => 2,
                _ => bail!("brightness must be 0–2 (low, medium, high)"),
            },
            Self::Interval => match s.as_str() {
                "1" => 1,
                "4" => 4,
                _ => bail!("interval must be 1 or 4 (seconds)"),
            },
        })
    }

    /// Current value from a GET_CONFIG reply, in the same form `parse` returns.
    pub fn current(self, c: &Config) -> u8 {
        match self {
            Self::Spo2Reminder => c.spo2_vibrate as u8,
            Self::PulseReminder => c.pulse_vibrate as u8,
            Self::Spo2Threshold => c.spo2_threshold,
            Self::PulseLow => c.pulse_low,
            Self::PulseHigh => c.pulse_high,
            Self::Vibration => c.motor,
            Self::Display => c.display_mode,
            Self::Brightness => c.brightness,
            Self::Interval => c.interval_s,
        }
    }

    pub fn describe(self, v: u8) -> String {
        match self {
            Self::Spo2Reminder | Self::PulseReminder => (if v != 0 { "on" } else { "off" }).into(),
            Self::Spo2Threshold => format!("{v} %"),
            Self::PulseLow | Self::PulseHigh => format!("{v} bpm"),
            Self::Display => match v {
                0 => "standard".into(),
                2 => "always-on".into(),
                m => format!("mode {m}"),
            },
            Self::Brightness => match v {
                0 => "0 (low)".into(),
                1 => "1 (medium)".into(),
                2 => "2 (high)".into(),
                b => b.to_string(),
            },
            Self::Interval => format!("{v} s"),
            Self::Vibration => v.to_string(),
        }
    }

    /// Value to put on the wire. Reminder switches carry vibrate in bit 0 and sound in bit 1
    /// (although GET_CONFIG reports pulse in bits 4/5), so the sound bit is preserved.
    pub fn wire_value(self, desired: u8, c: &Config) -> u16 {
        match self {
            Self::Spo2Reminder => (desired as u16) | if c.spo2_sound { 2 } else { 0 },
            Self::PulseReminder => (desired as u16) | if c.pulse_sound { 2 } else { 0 },
            _ => desired as u16,
        }
    }

    /// Cross-setting checks that need the current config.
    pub fn check_against(self, desired: u8, c: &Config) -> Result<()> {
        match self {
            Self::PulseLow => ensure!(
                desired < c.pulse_high,
                "pulse-low {desired} must be below pulse-high ({} bpm)",
                c.pulse_high
            ),
            Self::PulseHigh => ensure!(
                desired > c.pulse_low,
                "pulse-high {desired} must be above pulse-low ({} bpm)",
                c.pulse_low
            ),
            _ => {}
        }
        Ok(())
    }
}

/// After a write: the setting must now read back as `desired`, and nothing else in the
/// config may have changed.
pub fn check_readback(s: Setting, desired: u8, before: &Config, after: &Config) -> Result<()> {
    let got = s.current(after);
    ensure!(
        got == desired,
        "{} didn't take: ring reports {} after the write (wanted {})",
        s.name(),
        s.describe(got),
        s.describe(desired)
    );
    let (idx, mask) = s.slot();
    let n = before.raw.len().max(after.raw.len());
    let mut changed = Vec::new();
    for i in 0..n {
        let keep = if i == idx { !mask } else { 0xFF };
        let (b, a) = (before.raw.get(i).copied(), after.raw.get(i).copied());
        if b.map(|v| v & keep) != a.map(|v| v & keep) {
            changed.push(format!("byte {i}: {b:02x?} → {a:02x?}"));
        }
    }
    ensure!(
        changed.is_empty(),
        "{} took, but other settings changed too: {}",
        s.name(),
        changed.join(", ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::parse_config;

    fn cfg(bytes: &[u8]) -> Config {
        let mut raw = vec![0u8; 40];
        raw[..bytes.len()].copy_from_slice(bytes);
        parse_config(&raw).unwrap()
    }

    #[test]
    fn range_checks() {
        assert_eq!(Setting::Spo2Threshold.parse("89").unwrap(), 89);
        assert!(Setting::Spo2Threshold.parse("8").is_err());
        assert!(Setting::Spo2Threshold.parse("96").is_err());
        assert_eq!(Setting::PulseLow.parse("45").unwrap(), 45);
        assert!(Setting::PulseLow.parse("47").is_err());
        assert!(Setting::PulseHigh.parse("205").is_err());
        assert!(Setting::Vibration.parse("50").is_err());
        assert_eq!(Setting::Display.parse("Always-On").unwrap(), 2);
        assert_eq!(Setting::Brightness.parse("high").unwrap(), 2);
        assert!(Setting::Interval.parse("2").is_err());
        assert_eq!(Setting::Spo2Reminder.parse("off").unwrap(), 0);
        assert!(Setting::Spo2Reminder.parse("yes").is_err());
    }

    #[test]
    fn cross_checks() {
        let c = cfg(&[0x01, 88, 50, 120]);
        assert!(Setting::PulseLow.check_against(70, &c).is_ok());
        assert!(Setting::PulseHigh.check_against(50, &c).is_err());
    }

    #[test]
    fn wire_value_keeps_sound_bit() {
        let c = cfg(&[0x01 | 0x02 | 0x20, 88, 50, 120]);
        assert_eq!(Setting::Spo2Reminder.wire_value(0, &c), 2);
        assert_eq!(Setting::PulseReminder.wire_value(1, &c), 3);
        assert_eq!(Setting::Spo2Threshold.wire_value(89, &c), 89);
    }

    #[test]
    fn readback_detects_failure_and_collateral_change() {
        let before = cfg(&[0x01, 88, 50, 120, 60]);
        let ok = cfg(&[0x01, 89, 50, 120, 60]);
        assert!(check_readback(Setting::Spo2Threshold, 89, &before, &ok).is_ok());
        let unchanged = cfg(&[0x01, 88, 50, 120, 60]);
        assert!(check_readback(Setting::Spo2Threshold, 89, &before, &unchanged).is_err());
        let collateral = cfg(&[0x01, 89, 50, 120, 80]);
        assert!(check_readback(Setting::Spo2Threshold, 89, &before, &collateral).is_err());
        // Toggling the SpO₂ reminder may change bit 0 only, not the pulse bit in the same byte.
        let toggled = cfg(&[0x00, 88, 50, 120, 60]);
        assert!(check_readback(Setting::Spo2Reminder, 0, &before, &toggled).is_ok());
        let both = cfg(&[0x10, 88, 50, 120, 60]);
        assert!(check_readback(Setting::Spo2Reminder, 0, &before, &both).is_err());
    }
}
