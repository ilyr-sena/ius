# meridian-py

Modular Python orchestrator for the Meridian iOS relay.

Replaces `tools/ius.py`: same feature set, but structured as a real package
with device registry, per-service supervisor, layered config, and tests.

## Install

```sh
pip install -e .
# or without install:
PYTHONPATH=src python3 -m meridian_py --help
```

## Subcommands

```
meridian-py list       # attached devices + previously-seen registry entries (with friendly names)
meridian-py info       # full lockdown metadata for a device
meridian-py watch      # attach/detach event stream
meridian-py run        # orchestrated session: supervisor + WDA + probe + HTTP API on :9001
meridian-py tunnel     # iproxy-style port tunnels
```

`list` merges live mux devices with the persistent registry — replugged and
offline-known phones both show up, with correct iPhone/iPad model names.

### `run` flags

```
--no-wda        skip WebDriverAgent
--no-hid        skip HID gesture worker
--no-tunnels    skip iproxy-style port forwarding
--no-stream     skip the H.264 stream app launch
--no-http       skip the :9001 HTTP API
```

## Architecture

```
src/meridian_py/
├── cli.py            # subcommand routing only
├── commands.py       # list/info/watch leaf commands
├── config.py         # layered config: defaults ← TOML ← CLI
├── log.py            # structured colored logging
├── runner.py         # Session orchestration (run subcommand)
├── models.py         # Device/DeviceInfo + model name tables
├── mux/
│   ├── client.py     # usbmuxd wire protocol
│   ├── lockdown.py   # lockdown proxying via pymobiledevice3
│   └── tunnel.py     # iproxy-replacement TCP tunnels
├── devices/
│   ├── registry.py   # persistent JSON device registry
│   └── watcher.py    # attach/detach listener
├── services/
│   ├── supervisor.py # subprocess supervision w/ backoff
│   ├── tunneld.py    # pymobiledevice3 tunnel daemon
│   └── iproxy.py
├── stream/
│   ├── h264.py       # H.264 stream probe launcher
│   └── wda.py        # WDA session client
└── server/
    └── http.py       # HTTP/JSON control API
```

## State

Device knowledge persists to `~/.local/state/meridian/devices.json`. Config
lands in `~/.config/meridian/config.toml` (paths, bundle IDs, ports).

## License

GPL-2.0-or-later.
