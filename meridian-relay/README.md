# meridian-relay

A Rust reimplementation of Apple's `usbmuxd` — a userspace daemon that
multiplexes TCP-over-USB connections to iOS devices (iPhone / iPad).

It speaks the usbmuxd client protocol so standard libimobiledevice tooling
(`idevicepair`, `ideviceinfo`, ...) can talk to devices through it.
**One codebase builds for Linux and Windows.**

| Platform | IPC endpoint | Usage |
|---|---|---|
| Linux/macOS | Unix socket `/var/run/usbmuxd` | `meridian-relay daemon` |
| Windows | Named pipe `\\.\pipe\meridian-relay` | `meridian-relay.exe daemon` |
| Any | Loopback TCP (opt-in) | `-e tcp:127.0.0.1:27015` |

Specify endpoints as `unix:/path`, `pipe:name`, or `tcp:addr:port`
(`--endpoint` flag or `endpoint = "..."` in the config file).
`--socket-path` remains as a legacy alias for a unix endpoint.
`USBMUXD_SOCKET_ADDRESS` still overrides for client commands.

## Features

- **Full USB mux data path in Rust** — USB bulk I/O via libusb, mux frame
  parsing, version negotiation (v1/v2), and a built-in TCP-over-USB stack
  (SYN/SYN-ACK handshake, RST/FIN teardown, receive-window flow control).
- **usbmuxd-compatible socket protocol** — `ListDevices`, `Listen`,
  `Connect`, `ReadBUID`, `ReadPairRecord`, `SavePairRecord`,
  `DeletePairRecord`, result codes match usbmuxd conventions.
- **Security-hardened** — peer UID allowlists (`SO_PEERCRED`), UDID
  validation + path-traversal defenses, `O_NOFOLLOW` pair-record writes with
  `0600` perms, packet size caps, per-connection buffer caps with overflow
  accounting.
- **Operable** — JSON or pretty logs, live stats (`MeridianStats` command /
  `meridian-relay stats`), graceful SIGINT/SIGTERM shutdown, socket cleanup,
  systemd `READY=1` notification.
- **Layered config** — TOML file, then CLI flags, validated at startup.

## Requirements

- Rust stable (edition 2024)
- **Linux**: libusb-1.0 development files (`libusb-1.0-0-dev` / `libusbx-devel`)
- **Windows**: no system dependencies (libusb is compiled statically via the
  `vendored` feature); devices must be bound to WinUSB — see
  [Windows setup](#windows-setup)
- `ideviceinfo` (libimobiledevice) for device info enrichment

## Build & test

```sh
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

Cross-builds for Windows from Linux work with the gnu target:

```sh
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

## Usage

```sh
# Run the daemon (defaults to /var/run/usbmuxd — usually as root or a
# dedicated service user with udev permissions for USB access)
meridian-relay daemon
meridian-relay daemon --config /etc/meridian-relay.toml

# Print the effective configuration and exit
meridian-relay daemon --print

# Query a running daemon
meridian-relay list                 # table of attached devices
meridian-relay list --json
meridian-relay info [udid]
meridian-relay watch [udid]         # live attach/detach events
meridian-relay stats [-n 10]        # daemon metrics snapshot(s)
```

All commands accept `--socket-path` / `USBMUXD_SOCKET_ADDRESS` to point at a
custom daemon socket.

## Configuration

Every setting has a built-in default; a TOML file overrides defaults, CLI
flags override the file.

```toml
# /etc/meridian-relay.toml            (Linux default path)
# %ProgramData%\Meridian\config.toml  (Windows default path)
endpoint         = "unix:/var/run/usbmuxd"   # "pipe:meridian-relay" on Windows
socket_mode      = "0660"            # octal — unix sockets only
socket_group     = "usbmux"          # socket group ownership — unix only
pipe_security    = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;IU)"  # windows SDDL
lockdown_dir     = "/var/lib/lockdown"  # %ProgramData%\Apple\Lockdown on Windows
require_pair_record = true           # refuse lockdown connects without a pair record
scan_interval_ms = 2000
usb_timeout_ms   = 5000
connect_timeout_ms = 5000
max_clients      = 256
allowed_uids     = [0, 1000]         # unix peers; empty = allow all
allowed_sids     = []                # windows SIDs; empty = allow all
log_format       = "text"            # or "json"
```

Startup validation rejects zero/zero-ish values and insecure socket modes
emit warnings.

## Security notes

- Pair records are the keys to the device: writes are symlink-safe
  (`O_NOFOLLOW`), UID-validated filenames only, always `0600`.
- Use `allowed_uids` + a `socket_group`/`socket_mode` (e.g. `0660`) to
  restrict who can talk to the daemon in production.
- `require_pair_record = true` (default) blocks unpaired clients from opening
  lockdown (port 62078).

## Linux setup (one command)

```sh
sudo meridian-relay setup
meridian-relay daemon        # or: sudo systemctl start meridian-relay
```

`setup` installs the udev rule (USB `TAG+="uaccess"` for session access) and
the hardened systemd unit (`packaging/meridian-relay.service`), with
`Type=notify` readiness and sandboxing. Use `--skip-service` to skip the unit.

## systemd

```sh
sudo cp packaging/meridian-relay.service /etc/systemd/system/
sudo systemctl enable --now meridian-relay.service
```

The service uses `Type=notify` (the daemon sends `READY=1` once bound) and
sandboxing directives; see the unit file.

## Windows setup (one command)

In an **elevated** terminal:

```powershell
meridian-relay.exe setup
```

This is fully self-contained and idempotent. It:
1. extracts the embedded WinUSB **INF** (matched by the Apple mux interface
   class triple `FF/FE/02` — covers all iPhone/iPad models and modes),
2. stages it into the driver store via `pnputil` (every *future* device
   attaches to WinUSB automatically),
3. rebinds **already-attached** devices immediately via
   `UpdateDriverForPlugAndPlayDevices` (no replug needed),
4. installs the auto-start Windows service (drop with `--skip-service`).

Then:

```powershell
sc.exe start meridian-relay
meridian-relay.exe list     # or: watch / info / stats
```

While a device is WinUSB-bound, iTunes won't see it; running
`pnputil /delete-driver oem<N>.inf /uninstall /force` (or Device Manager →
update driver → Apple) restores iTunes access.

### No drivers at all? Use relay mode (Windows, free)

If you can't/won't bind drivers — e.g. locked-down enterprise machines —
meridian-relay can run as a **transparent relay to Apple's own mux service**
(the free "Apple Mobile Device Service" that ships with iTunes or the
[Apple Devices](https://aka.ms/AppleDevices) Store app):

```powershell
meridian-relay.exe daemon --backend relay
# or with a custom upstream:
meridian-relay.exe daemon --backend relay --upstream tcp:127.0.0.1:27015
```

Every client command (`list`, `connect`, pair-record CRUD, raw proxies, the
same endpoint for third-party tools) passes through unchanged. Pair-record
enforcement, peer allowlists, metrics, and stats still apply at our edge.
Config file equivalents: `backend = "relay"`, `upstream = "tcp:127.0.0.1:27015"`.

`--backend relay` works on Linux too (e.g. to chain relays across hosts).

Manual fallback: `meridian-relay.exe service install|uninstall` only touches
the service; the INF can also be deployed by hand from
`packaging/meridian-relay-winusb.inf`.

### Windows-specific configuration

| Setting | Purpose |
|---|---|
| `pipe_security` | SDDL DACL for the named pipe (default: SYSTEM + Admins + interactive users) |
| `allowed_sids` | Allowlist of client user/group SIDs (empty = allow all) |
| `lockdown_dir` | defaults to `%ProgramData%\Apple\Lockdown` (iTunes-compatible) |
| config file | auto-discovered at `%ProgramData%\Meridian\config.toml` |

`socket_mode`, `socket_group`, and `allowed_uids` are unix-only and ignored
on Windows (`--pipe-security` + `--allow-sid` are their Windows equivalents).

## Layout

```
src/
  config.rs      layered, validated, cross-platform configuration
  metrics.rs     atomic counters + JSON snapshot
  security.rs    UDID validation, peer creds, RAII guards
  platform/      ALL OS divergence lives here (unix.rs / windows.rs)
  service.rs     Windows SCM integration (windows only)
  device/        client-side query/enrich/watch (CLI)
  daemon/
    transport.rs       endpoint abstraction: unix socket / named pipe / TCP
    protocol.rs        usbmuxd socket framing + result codes
    device_scanner.rs  USB enumeration via libusb
    device_manager.rs  per-device async task, connect dispatch
    connection.rs      client command handling + proxy loop
    mux.rs             mux framing, TCP-over-USB stack, reassembler
    usb.rs             bulk endpoint I/O (single ordered reader)
tests/integration.rs  end-to-end socket/pipe-level tests
test_client.py        manual smoke test against a live device
packaging/
  meridian-relay.service     hardened systemd unit (Linux)
  meridian-relay-winusb.inf  WinUSB driver binding (Windows)
```

## Test client

With the daemon running and a device attached:

```sh
./test_client.py
```
