# o2ring-cli

Download, inspect and configure a **Viatom O2Ring S** pulse oximeter ring from Linux over
Bluetooth. No browser, phone app or account needed.

`o2ring` talks to the ring's vendor Bluetooth service (the "OxyII" protocol) directly through
BlueZ, saves each recording as the ring's original file plus a [Parquet](https://parquet.apache.org/)
copy for analysis, and can show live readings or change the ring's settings.

> **Not a medical device.** This is an unofficial tool for personal use. It isn't affiliated
> with Viatom/Wellue, and nothing it shows is a diagnosis.

## Supported devices

Tested on an **O2Ring S** (advertises as `S8-AW …`), firmware **1.13.1.0**. Newer firmware like
this encrypts the session (AES-128); older 1.0.x firmware doesn't, and both are handled.
Other OxyII-family devices may work but are untested.

## Requirements

- Linux with **BlueZ** (`bluetoothd`), which every major desktop distribution ships and enables
- A Bluetooth LE adapter
- To build: Rust (stable), `pkg-config` and the D-Bus headers
  (`sudo apt install libdbus-1-dev pkg-config` on Debian/Ubuntu)

No pairing is needed. Only one app can be connected to the ring at a time, so close the
phone app (ViHealth) or turn off the phone's Bluetooth, and close any browser tool connected
to it.

## Build and install

```sh
cargo install --path .     # installs ~/.cargo/bin/o2ring
```

or `cargo build --release` and use `target/release/o2ring`.

## Usage

The ring only advertises while it's awake. Wear it or tap its screen first.

| Command | What it does |
|---|---|
| `o2ring sync` | Download recordings not yet in the folder; save the raw file and a `.parquet` |
| `o2ring sync --verify` | Also re-download existing recordings and check they're byte-identical (never overwrites) |
| `o2ring sync --dry-run` | Only list what would be downloaded |
| `o2ring info` | Serial, firmware, clock, battery, and the recordings stored on the ring |
| `o2ring config` | Current settings (`--raw` adds the reply bytes) |
| `o2ring live` | Live SpO₂, pulse, perfusion index and motion, once a second (`--seconds N` or Ctrl-C) |
| `o2ring set <setting> <value>` | Change one setting, then read it back to confirm |
| `o2ring set-clock` | Set the ring's clock to this computer's local time and time zone |
| `o2ring convert [files]` | Convert raw recordings to Parquet without the ring |

Common options:

- `--dir <folder>` or `O2RING_DIR`: the recordings folder (default `./recordings`)
- `--address <AA:BB:…>` or `O2RING_ADDRESS`: connect to a specific ring instead of the first
  one advertising as `S8-AW…` / `O2Ring…`
- `--scan-timeout <s>`: how long to look for the ring (default 20)

Settings for `o2ring set`:

| Setting | Allowed values |
|---|---|
| `spo2-reminder` | `on`, `off` (vibrate when SpO₂ drops below the threshold) |
| `spo2-threshold` | 80–95 (%) |
| `pulse-reminder` | `on`, `off` |
| `pulse-low` | 30–70 bpm, steps of 5 (must stay below `pulse-high`) |
| `pulse-high` | 70–200 bpm, steps of 5 (must stay above `pulse-low`) |
| `vibration` | 20, 40, 60, 80, 100 |
| `display` | `standard`, `always-on` |
| `brightness` | 0–2, or `low`, `medium`, `high` |
| `interval` | 1 or 4 seconds (see safeguard 5) |

### Example

```text
$ o2ring sync --dir ~/oximetry
scanning for the ring…
connecting to S8-AW 701234 (…)…
connected (encrypted session), battery 95% (normal)
have   20261108170539
saved  20261108235152  2026-11-08T23:51:52  8h44m29s  SpO2 avg 96% min 89%  pulse avg 69  drops ≥3% 40 ≥4% 15, ODI4 1.7/h  <90% 8s  O2 score 9.6
1 new recording(s) saved

$ o2ring set spo2-threshold 89
spo2-threshold 88 % → 89 % ✓
```

## Safeguards

A pulse oximeter holds health data, and some of the ring's commands can erase it. `o2ring` is
built so that mistakes, bugs and unattended runs can't damage the ring or its recordings.

### Commands that are never sent

The protocol includes commands to reset the ring, factory-reset it (which **erases all
recordings**), update firmware, write or delete files, and factory programming. None of them
are in `o2ring`'s allowlists, so it can't send them, whatever the input. A unit test checks
that every one is refused.

### 1. Read-only connections by default

Every connection is opened in one of two modes. `sync`, `info`, `config` and `live` open
**read-only** connections that can only send read commands. Only `set` and `set-clock` open a
**write** connection that may also send the settings and clock commands. The check happens
before anything is sent over Bluetooth.

So `sync` can run unattended (for example from a timer) with no risk of changing the ring,
even if it had a bug.

### 2. Range checks before connecting

Every value given to `o2ring set` is checked against the values the vendor's own apps allow
(see the table above) **before** the ring is contacted. A typo such as `spo2-threshold 8`
fails locally with `spo2-threshold must be 80–95`, and nothing is sent. Cross-setting rules
are checked against the ring's current settings, for example that `pulse-low` stays below
`pulse-high`.

### 3. Read back after every write

The ring acknowledging a command doesn't prove the change took effect, especially with a
reverse-engineered protocol. After each write, `o2ring` reads all the settings again and
checks two things:

- the setting now has the new value, and
- **nothing else changed**. Every other byte of the settings must match what it was before,
  which catches a write that landed in the wrong place. (Even the SpO₂ and pulse reminders,
  which share a byte, are checked separately.)

If either check fails, the command reports an error instead of `✓`.

### 4. One setting per command

There's no command that writes a whole configuration at once, which could silently overwrite
settings you didn't mean to touch. Each `o2ring set` changes exactly one setting and shows
`old → new`, so every change is deliberate and easy to undo. If the setting already has the
requested value, nothing is sent.

### 5. Confirmation before reducing recording detail

The ring normally saves one reading every second. Setting `interval 4` makes it save one every
4 seconds. Short oxygen dips can then go unrecorded, so recordings look smoother than the night
really was, and drop counts and ODI can come out too low. Nothing looks wrong, and the lost
detail can't be recovered later.

So `o2ring set interval 4` refuses and explains this unless you add `--yes`. Changing back to
`interval 1` needs no confirmation.

### 6. No clock changes while recording

Recording timestamps come from the ring's own clock, so changing it mid-recording could leave
that recording's times inconsistent. `o2ring set-clock` checks the ring's live status first
and refuses while it is preparing or measuring, unless you add `--force`. Take the ring off and
wait about 2 minutes (until the recording is finished) before setting the clock.

### Also

- **Downloads are verified.** A recording is only saved after its magic number, checksum and
  sample count check out. A recording the ring hasn't finished writing (it adds the summary
  about 2 minutes after removal) is skipped with `wait` and picked up on the next run.
- **Existing files are never overwritten.** `sync` skips recordings already in the folder, and
  `--verify` only compares. Files are written to a temporary name and renamed, so an
  interrupted run never leaves a half-written recording.
- **Ring-supplied names are validated.** Only `yyyyMMddHHmmss` names are accepted, so a name
  from the device can't write outside the recordings folder.

## Output format

Each recording is saved under the name the ring gives it (its start time):

- `YYYYMMDDhhmmss`: the ring's original file, byte for byte. Keep this as the source of truth.
- `YYYYMMDDhhmmss.parquet`: one row per sample, zstd-compressed:

| Column | Type | Meaning |
|---|---|---|
| `time` | timestamp (ms, no time zone) | The ring's local clock |
| `spo2` | uint8, nullable | SpO₂ %, null when invalid |
| `pulse` | uint8, nullable | Pulse in bpm, null when invalid |
| `motion` | uint8 | Motion intensity, 0–63 |
| `spo2_reminder` | bool | The SpO₂ vibration reminder fired |
| `pulse_reminder` | bool | The pulse vibration reminder fired |

File metadata: `o2ring.summary` (JSON with the ring's own summary: average and minimum SpO₂,
drop counts, time below 90 %, O₂ score, …), `o2ring.source_file`, `o2ring.source_sha256` and
`o2ring.decoder_version`.

## Acknowledgements

The protocol and file-format knowledge comes from
[O2Ring Web](https://github.com/khromov/wellue-o2ring-web-ui) by khromov, whose
protocol notes document the OxyII framing, the commands and the recording format, and from the
reverse-engineering in [nglessner/o2ring-s-protocol](https://github.com/nglessner/o2ring-s-protocol).
`o2ring-cli` is an independent implementation in Rust and contains no code from those projects.

## License

[GNU Affero General Public License v3.0 or later](LICENSE).
