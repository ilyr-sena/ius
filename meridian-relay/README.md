# meridian-relay

A Rust reimplementation of Apple's `usbmuxd` — a userspace daemon that
multiplexes TCP connections to iOS devices (iPhone / iPad) over USB.

It speaks the usbmuxd client protocol on a Unix domain socket, so standard
libimobiledevice tooling (`idevicepair`, `ideviceinfo`, ...) can talk to
devices through it.

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

- Linux, Rust stable (edition 2024)
- libusb-1.0 development files (`libusb-1.0-0-dev` / `libusbx-devel`)
- `ideviceinfo` (libimobiledevice) for device info enrichment

## Build & test

```sh
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
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
# /etc/meridian-relay.toml
socket_path      = "/var/run/usbmuxd"
socket_mode      = "0660"            # octal
socket_group     = "usbmux"          # socket group ownership
lockdown_dir     = "/var/lib/lockdown"
require_pair_record = true           # refuse lockdown connects without a pair record
scan_interval_ms = 2000
usb_timeout_ms   = 5000
connect_timeout_ms = 5000
max_clients      = 256
allowed_uids     = [0, 1000]         # empty = allow all
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

## systemd

```sh
sudo cp packaging/meridian-relay.service /etc/systemd/system/
sudo systemctl enable --now meridian-relay.service
```

The service uses `Type=notify` (the daemon sends `READY=1` once bound) and
sandboxing directives; see the unit file.

## Layout

```
src/
  config.rs      layered, validated configuration
  metrics.rs     atomic counters + JSON snapshot
  security.rs    UDID validation, peer creds, RAII guards
  device/        client-side query/enrich/watch (CLI)
  daemon/
    protocol.rs        usbmuxd socket framing + result codes
    device_scanner.rs  USB enumeration via libusb
    device_manager.rs  per-device async task, connect dispatch
    connection.rs      client command handling + proxy loop
    mux.rs             mux framing, TCP-over-USB stack, reassembler
    usb.rs             bulk endpoint I/O (single ordered reader)
tests/integration.rs  end-to-end socket-level tests
test_client.py        manual smoke test against a live device
```

## Test client

With the daemon running and a device attached:

```sh
./test_client.py
```
