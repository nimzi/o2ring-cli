// SPDX-License-Identifier: AGPL-3.0-or-later
//! `o2ring`: download and convert Viatom O2Ring S recordings over Bluetooth, no browser needed.

mod crypto;
mod frame;
mod recording;
mod ring;
mod settings;

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use bluer::Address;
use clap::{Parser, Subcommand};

use crate::recording::{decode, sha256_hex, write_parquet, DecodeError};
use crate::ring::{Mode, Ring};
use crate::settings::{check_readback, Setting};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct Conn {
    /// Ring's Bluetooth address (default: the first device advertising as S8-AW… or O2Ring…)
    #[arg(long, env = "O2RING_ADDRESS")]
    address: Option<Address>,
    /// Seconds to scan for the ring before giving up
    #[arg(long, default_value_t = 20)]
    scan_timeout: u64,
}

#[derive(Subcommand)]
enum Command {
    /// Download recordings that aren't in the recordings folder yet, and convert them to Parquet
    Sync {
        /// Recordings folder (raw files + .parquet)
        #[arg(long, env = "O2RING_DIR", default_value = "recordings")]
        dir: PathBuf,
        #[command(flatten)]
        conn: Conn,
        /// Only show what would be downloaded
        #[arg(long)]
        dry_run: bool,
        /// Also re-download recordings that already exist locally and check they are
        /// byte-identical (never overwrites them)
        #[arg(long)]
        verify: bool,
    },
    /// Show device info, battery, and the recordings stored on the ring
    Info {
        #[command(flatten)]
        conn: Conn,
    },
    /// Show the ring's settings (read-only)
    Config {
        #[command(flatten)]
        conn: Conn,
        /// Also print the raw reply bytes
        #[arg(long)]
        raw: bool,
    },
    /// Show live SpO₂, pulse, PI and motion, about once a second (Ctrl-C to stop)
    Live {
        #[command(flatten)]
        conn: Conn,
        /// Stop after this many seconds (0 = until Ctrl-C)
        #[arg(long, default_value_t = 0)]
        seconds: u64,
    },
    /// Change one ring setting, then read it back to confirm (the only command besides
    /// set-clock that can change the ring)
    Set {
        #[arg(value_enum)]
        setting: Setting,
        /// New value (see `o2ring set --help` for each setting's allowed values)
        value: String,
        /// Confirm a change that reduces recording detail (interval 4)
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        conn: Conn,
    },
    /// Set the ring's clock to this computer's local time and time zone
    SetClock {
        /// Set it even while the ring is recording (can make that recording's times inconsistent)
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        conn: Conn,
    },
    /// Convert raw files to Parquet (default: every raw file in the folder without a .parquet)
    Convert {
        /// Raw recording files
        files: Vec<PathBuf>,
        #[arg(long, env = "O2RING_DIR", default_value = "recordings")]
        dir: PathBuf,
        /// Overwrite existing .parquet files
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Sync { dir, conn, dry_run, verify } => sync(&dir, &conn, dry_run, verify).await,
        Command::Info { conn } => info(&conn).await,
        Command::Config { conn, raw } => config(&conn, raw).await,
        Command::Live { conn, seconds } => live(&conn, seconds).await,
        Command::Set { setting, value, yes, conn } => set(&conn, setting, &value, yes).await,
        Command::SetClock { force, conn } => set_clock(&conn, force).await,
        Command::Convert { files, dir, force } => convert(files, &dir, force),
    }
}

/// Recording names are timestamps, `yyyyMMddHHmmss`. Anything else from the ring is ignored,
/// so a device-supplied name can never escape the recordings folder.
fn valid_name(name: &str) -> bool {
    name.len() == 14 && name.bytes().all(|b| b.is_ascii_digit())
}

async fn open(conn: &Conn, mode: Mode) -> Result<Ring> {
    let session = bluer::Session::new().await.context("can't reach BlueZ over D-Bus")?;
    let adapter = session.default_adapter().await.context("no Bluetooth adapter")?;
    if !adapter.is_powered().await? {
        bail!("Bluetooth adapter {} is powered off", adapter.name());
    }
    eprintln!("scanning for the ring…");
    let dev = ring::find(&adapter, conn.address, Duration::from_secs(conn.scan_timeout)).await?;
    let name = dev.name().await?.unwrap_or_default();
    eprintln!("connecting to {name} ({})…", dev.address());
    Ring::connect(dev, mode).await
}

async fn info(conn: &Conn) -> Result<()> {
    let mut r = open(conn, Mode::ReadOnly).await?;
    let res = async {
        let i = r.info().await?;
        let b = r.battery().await?;
        let files = r.list_files().await?;
        println!("serial    {}", i.serial);
        println!("firmware  {}", i.firmware);
        println!("clock     {}", i.clock);
        println!("battery   {}% ({})", b.percent, b.state);
        println!("recordings on ring ({}):", files.len());
        for f in files {
            println!("  {f}");
        }
        anyhow::Ok(())
    }
    .await;
    r.disconnect().await;
    res
}

async fn config(conn: &Conn, raw: bool) -> Result<()> {
    let mut r = open(conn, Mode::ReadOnly).await?;
    let res = r.config().await;
    r.disconnect().await;
    let c = res?;
    let on = |b: bool| if b { "on" } else { "off" };
    println!("SpO₂ reminder     vibrate {}, sound {}", on(c.spo2_vibrate), on(c.spo2_sound));
    println!("SpO₂ threshold    {} %", c.spo2_threshold);
    println!("pulse reminder    vibrate {}, sound {}", on(c.pulse_vibrate), on(c.pulse_sound));
    println!("pulse low / high  {} / {} bpm", c.pulse_low, c.pulse_high);
    println!("vibration         {}", c.motor);
    println!("buzzer            {}", c.buzzer);
    let display = match c.display_mode {
        0 => "standard".to_string(),
        2 => "always on".to_string(),
        m => format!("mode {m}"),
    };
    println!("display           {display}");
    println!("brightness        {}", c.brightness);
    println!("storage interval  {} s", c.interval_s);
    if let Some(h) = c.utc_offset_h {
        println!("UTC offset        {h:+.1} h");
    }
    if raw {
        let hex: Vec<String> = c.raw.iter().map(|b| format!("{b:02x}")).collect();
        println!("raw ({} bytes)    {}", c.raw.len(), hex.join(" "));
    }
    Ok(())
}

async fn live(conn: &Conn, seconds: u64) -> Result<()> {
    let mut r = open(conn, Mode::ReadOnly).await?;
    let stop = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let res: Result<()> = async {
        println!("time      status     sensor     SpO₂  pulse   PI    motion  battery");
        loop {
            let t0 = tokio::time::Instant::now();
            let s = r.live().await?;
            let opt = |v: Option<String>| v.unwrap_or_else(|| "–".into());
            println!(
                "{}  {:<9}  {:<9}  {:>4}  {:>5}  {:>4}  {:>6}  {:>3}% {}",
                chrono::Local::now().format("%H:%M:%S"),
                s.status,
                s.sensor,
                opt(s.spo2.map(|v| format!("{v}%"))),
                opt(s.pulse.map(|v| v.to_string())),
                opt(s.pi.map(|v| format!("{v:.1}"))),
                opt(s.motion.map(|v| v.to_string())),
                s.battery,
                s.battery_state,
            );
            if seconds > 0 && tokio::time::Instant::now() >= stop {
                return Ok(());
            }
            let wait = Duration::from_secs(1).saturating_sub(t0.elapsed());
            tokio::select! {
                _ = &mut ctrl_c => return Ok(()),
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }
    .await;
    r.disconnect().await;
    res
}

async fn set(conn: &Conn, s: Setting, value: &str, yes: bool) -> Result<()> {
    // Range-check before connecting: a bad value never reaches the ring.
    let desired = s.parse(value)?;
    if s == Setting::Interval && desired == 4 && !yes {
        bail!(
            "interval 4 makes every future recording less detailed: the ring would save one \
             reading every 4 seconds instead of every second, which can hide short oxygen drops \
             and make the drop counts and ODI come out too low. Add --yes if you really want this."
        );
    }
    let mut r = open(conn, Mode::Write).await?;
    let res = async {
        let before = r.config().await?;
        let cur = s.current(&before);
        if cur == desired {
            println!("{} is already {}; nothing sent", s.name(), s.describe(cur));
            return Ok(());
        }
        s.check_against(desired, &before)?;
        r.set_config(s.config_type(), s.wire_value(desired, &before)).await?;
        let after = r.config().await?;
        check_readback(s, desired, &before, &after)?;
        println!("{} {} → {} ✓", s.name(), s.describe(cur), s.describe(desired));
        anyhow::Ok(())
    }
    .await;
    r.disconnect().await;
    res
}

async fn set_clock(conn: &Conn, force: bool) -> Result<()> {
    let mut r = open(conn, Mode::Write).await?;
    let res = async {
        let l = r.live().await?;
        if (l.status == "preparing" || l.status == "measuring") && !force {
            bail!(
                "the ring is recording (status: {}); changing its clock now can make this \
                 recording's times inconsistent. Take it off, wait ~2 minutes, and try again \
                 (or add --force).",
                l.status
            );
        }
        let before = r.info().await?.clock;
        let now = chrono::Local::now();
        let method = r.set_clock(&now).await?;
        let after = r.info().await?.clock;
        println!("ring clock before  {before}");
        println!("set to             {} (UTC{}) via {method}", now.format("%Y-%m-%d %H:%M:%S"), now.format("%:z"));
        println!("ring clock now     {after}");
        println!("system clock       {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
        anyhow::Ok(())
    }
    .await;
    r.disconnect().await;
    res
}

async fn sync(dir: &Path, conn: &Conn, dry_run: bool, verify: bool) -> Result<()> {
    if !dir.is_dir() {
        bail!("recordings folder {} doesn't exist (use --dir or O2RING_DIR)", dir.display());
    }
    let mut r = open(conn, Mode::ReadOnly).await?;
    let res = sync_files(&mut r, dir, dry_run, verify).await;
    r.disconnect().await;
    res
}

async fn sync_files(r: &mut Ring, dir: &Path, dry_run: bool, verify: bool) -> Result<()> {
    let b = r.battery().await?;
    eprintln!(
        "connected ({}), battery {}% ({})",
        if r.encrypted() { "encrypted session" } else { "plaintext session" },
        b.percent,
        b.state
    );
    let files = r.list_files().await?;
    let (mut new, mut failed) = (0, 0);
    for name in files {
        if !valid_name(&name) {
            eprintln!("skip   {name:?}: not a recording name");
            continue;
        }
        let path = dir.join(&name);
        let exists = path.exists();
        if exists && !verify {
            println!("have   {name}");
            continue;
        }
        if dry_run {
            println!("{}  {name}", if exists { "verify" } else { "new   " });
            continue;
        }
        let data = r
            .read_file(&name, |done, total| {
                eprint!("\rread   {name}  {done}/{total} bytes");
                let _ = std::io::stderr().flush();
            })
            .await
            .with_context(|| format!("downloading {name}"))?;
        eprintln!();

        if exists {
            let local = std::fs::read(&path)?;
            if local == data {
                println!("same   {name}  sha256 {}", sha256_hex(&data));
            } else {
                failed += 1;
                println!(
                    "DIFFER {name}  ring {} bytes sha256 {}, local {} bytes sha256 {}",
                    data.len(),
                    sha256_hex(&data),
                    local.len(),
                    sha256_hex(&local)
                );
            }
            continue;
        }

        let rec = match decode(&data) {
            Ok(rec) => rec,
            Err(DecodeError::NotFinalised) => {
                println!("wait   {name}: still recording, or taken off under ~2 min ago; try again later");
                continue;
            }
            Err(e) => {
                failed += 1;
                println!("FAIL   {name}: {e}");
                continue;
            }
        };
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, &path)?;
        write_parquet(&rec, &data, &name, &path.with_extension("parquet"))?;
        new += 1;
        println!("saved  {name}  {}", summary_line(&rec.summary));
    }
    if !dry_run {
        eprintln!("{new} new recording(s) saved");
    }
    if failed > 0 {
        bail!("{failed} recording(s) failed");
    }
    Ok(())
}

fn summary_line(s: &recording::Summary) -> String {
    let opt = |v: Option<u8>| v.map_or("–".to_string(), |v| v.to_string());
    let hours = s.duration_s as f64 / 3600.0;
    let odi = if hours >= 1.0 {
        format!(", ODI4 {:.1}/h", s.drops_4pct as f64 / hours)
    } else {
        String::new()
    };
    format!(
        "{}  {}h{:02}m{:02}s  SpO2 avg {}% min {}%  pulse avg {}  drops ≥3% {} ≥4% {}{}  <90% {}s  O2 score {}",
        s.start_local,
        s.duration_s / 3600,
        s.duration_s / 60 % 60,
        s.duration_s % 60,
        opt(s.avg_spo2),
        opt(s.min_spo2),
        s.avg_pulse,
        s.drops_3pct,
        s.drops_4pct,
        odi,
        s.sec_below_90,
        s.o2_score.map_or("–".to_string(), |v| format!("{v:.1}")),
    )
}

fn convert(files: Vec<PathBuf>, dir: &Path, force: bool) -> Result<()> {
    let files = if files.is_empty() {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.is_file() && p.file_name().and_then(|n| n.to_str()).is_some_and(valid_name)
            })
            .collect();
        v.sort();
        v
    } else {
        files
    };
    let mut failed = 0;
    for path in files {
        let out = path.with_extension("parquet");
        if out.exists() && !force {
            println!("skip   {} (parquet exists; --force to rebuild)", path.display());
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let result = std::fs::read(&path).map_err(anyhow::Error::from).and_then(|raw| {
            let rec = decode(&raw)?;
            write_parquet(&rec, &raw, &name, &out)?;
            Ok(rec)
        });
        match result {
            Ok(rec) => println!("wrote  {}  {}", out.display(), summary_line(&rec.summary)),
            Err(e) => {
                failed += 1;
                eprintln!("FAIL   {}: {e:#}", path.display());
            }
        }
    }
    if failed > 0 {
        bail!("{failed} file(s) failed");
    }
    Ok(())
}
