# meridian-relay — working state snapshot

> Frozen at commit ~`967d059` (main branch, `ius` repo). Resume from here.

## What's DONE and production-verified

- **Cross-platform daemon** (Linux + Windows from one codebase), 94 tests green.
- **Transport abstraction**: `unix:/pipe:/tcp:` endpoints, named-pipe ACLs (SDDL), loopback enforcement, `SocketCleanupGuard`, systemd `READY=1`, Windows SCM service (`service install/uninstall`).
- **Relay backend** (`--backend relay`): transparent splice to any upstream usbmuxd-compatible service. Works on Windows against Apple's `AppleMobileDeviceService` (tcp:127.0.0.1:27015), no driver needed. Verified with real iPhone through it.
- **USB device manager + scanner**: attach/detach with flap suppression, `(bus,address)` keyed, reconcile loop, correct detach cleanup.
- **`list`/`info`/`watch`/`stats`/`tunnel` CLI**. `tunnel` replaces iproxy and works.
- **Client-side lockdown**: `Connect` → `ReadPairRecord` → `StartSession` → optional TLS → `GetValue`, framed as BE-u32-length-prefixed XML plists. Verified against real device over relay path (relay does NOT exercise our own USB TCP stack).
- Config layering (defaults → TOML → CLI), `usb_timeout`/`max_packet_bytes`/`max_conn_buffer` all wired, fail-closed UID/SID authz.

## What's IN FLIGHT (this is the unfinished piece)

**The raw-USB mux data path** (`backend = default "usb"` without relay). SYN-handshake completes (connection established), and then the device RSTs with `no matching session`, and/or warns "duplicate packet / duplicate ack". Enrichment gets nothing.

Files to touch: `src/daemon/mux.rs`, `src/daemon/device_manager.rs`.

Working references (already present on this machine):
- `/tmp/usbmuxd-ref/src/device.c` — libimobiledevice usbmuxd (clone of upstream)
- `/tmp/usbmuxd2/usbmuxd2/` — tihmstar's usbmuxd2, with TCP.cpp flow control

Ground truth from the reference:
- Header v2 is 16 bytes: `proto u32 | length u32 | magic=0xFEEDFACE | tx_seq u16 | rx_seq u16`
- SETUP payload is `[0x07]`, and on setup: `tx_seq=0, rx_seq=0xFFFF`
- After every frame you send, `tx_seq += 1` — device's complaint `expected 2 received 1` was because my ACK after SYN-ACK reused the SYN's seq
- Window starts at 131072 (>>8 on the wire)
- Frame-parse behavior on receive: if v2, update `rx_seq` from the incoming header

Reproduce the failure: `./target/debug/meridian-relay -e unix:/tmp/mr-usb/mux.sock daemon` with the phone plugged in, then in another console: `./target/debug/meridian-relay -e unix:/tmp/mr-usb/mux.sock info`. Expected symptom: "connect failed: connection dead" seconds after `connection established on sport=1024`. Wire dump is captured into `/tmp/mr-usb/daemon.log` via the `usb_rx N bytes: [..]` INFO lines in `device_manager.rs`.

Likely next steps (didn't try yet): seq accounting per-frame as above; remove the ACK we send on SYN-ACK or fix its tx_seq; then re-verify against hardware.

## Windows build + serve
- Binary: `/var/tmp/opencode/win-target/x86_64-pc-windows-gnu/release/meridian-relay.exe`
- Served at `http://192.168.0.187:8777/meridian-relay.exe` (server runs detached via setsid; systemd unit planned, see below).

## IPA (probe app)
- CI workflow: `.github/workflows/probe-build.yml` — builds on `xcode-27` runner, uploads `IUSProbe-unsigned-ipa` artifact.
- Serve: `/var/tmp/opencode/http-serve/IUSProbe-unsigned.ipa` (same :8777 server).

## Pending minor items
- HTTP file server should become a systemd user service (it dies on reboot).
- `ideviceinfo` external fallback still present in `device/info.rs` (kept deliberately as gap-filler; works fine).
- Meridian-side pair-record mbedtls/SecureTransport handshake equivalents on Windows for relay-mode `GetValue` — works over relay already by virtue of upstream; needs no work unless USB backend is finalized.
