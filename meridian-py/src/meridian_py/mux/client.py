"""usbmuxd wire protocol client.

Speaks to any usbmuxd-compatible endpoint (meridian-relay daemon, Apple's
AppleMobileDeviceService on :27015, or the system Linux usbmuxd).
"""

from __future__ import annotations

import plistlib
import socket
import struct
from typing import Optional

from ..models import Device

USBMUXD_VERSION_PLIST = 1
USBMUXD_MSGTYPE_PLIST = 8


class MuxError(Exception):
    pass


class MuxClient:
    def __init__(self, endpoint: str):
        """`endpoint` is a unix socket path or `host:port`."""
        self.endpoint = endpoint

    def _connect(self) -> socket.socket:
        if "/" in self.endpoint or self.endpoint.startswith("unix:"):
            # Unix domain socket
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(self.endpoint.replace("unix:", ""))
        else:
            host, _, port = self.endpoint.rpartition(":")
            if not port:
                raise MuxError(f"bad endpoint '{self.endpoint}'")
            s = socket.create_connection((host, int(port)), timeout=10)
        s.settimeout(10)
        return s

    @staticmethod
    def _pack(tag: int, payload: dict) -> bytes:
        body = plistlib.dumps(payload, fmt=plistlib.FMT_XML)
        header = struct.pack("<IIII", len(body) + 16, USBMUXD_VERSION_PLIST, USBMUXD_MSGTYPE_PLIST, tag)
        return header + body

    @staticmethod
    def _recv(sock: socket.socket) -> dict:
        head = b""
        while len(head) < 16:
            chunk = sock.recv(16 - len(head))
            if not chunk:
                raise MuxError("connection closed by mux")
            head += chunk
        size, version, mtype, tag = struct.unpack("<IIII", head)
        if size < 16:
            raise MuxError(f"invalid frame size {size}")
        body = b""
        while len(body) < size - 16:
            chunk = sock.recv(size - 16 - len(body))
            if not chunk:
                raise MuxError("connection closed mid-payload")
            body += chunk
        m = plistlib.loads(body)
        if not isinstance(m, dict):
            raise MuxError("response not a dict")
        return m

    # ---- usbmuxd commands --------------------------------------------------

    def hello(self, tag: int = 1) -> dict:
        return self.request({"MessageType": "Hello"}, tag=tag)

    def list_devices(self, tag: int = 1) -> list[Device]:
        resp = self.request({
            "MessageType": "ListDevices",
            "ClientVersionString": "meridian-py",
            "ProgName": "meridian-py",
            "kLibUSBMuxVersion": 3,
        }, tag=tag)
        out = []
        for ent in resp.get("DeviceList", []):
            if not isinstance(ent, dict):
                continue
            props = ent.get("Properties", {})
            ud = props.get("SerialNumber") or props.get("UDID")
            did = props.get("DeviceID", 0)
            pid = props.get("ProductID", 0) or 0
            if ud:
                out.append(Device(
                    udid=str(ud).strip("\x00"),
                    device_id=int(did),
                    product_id=int(pid),
                ))
        return out

    def listen(self) -> "socket.socket":
        """Return an open socket that streams Attach/Detach events."""
        s = self._connect()
        req = {
            "MessageType": "Listen",
            "ClientVersionString": "meridian-py",
            "ProgName": "meridian-py",
            "kLibUSBMuxVersion": 3,
        }
        s.sendall(self._pack(1, req))
        first = self._recv(s)
        # First response is Result=OK
        if first.get("Number", 1) != 0:
            raise MuxError(f"listen rejected: {first}")
        return s

    def connect(self, device_id: int, port: int, tag: int = 2) -> socket.socket:
        """Ask the mux to splice this socket to a device port."""
        s = self._connect()
        req = {
            "MessageType": "Connect",
            "DeviceID": device_id,
            "PortNumber": int.from_bytes(struct.pack(">H", port), "little"),
            "ClientVersionString": "meridian-py",
            "ProgName": "meridian-py",
            "kLibUSBMuxVersion": 3,
        }
        s.sendall(self._pack(tag, req))
        resp = self._recv(s)
        if resp.get("Number", 1) != 0:
            s.close()
            raise MuxError(f"connect refused: {resp}")
        return s

    def read_pair_record(self, udid: str) -> dict:
        resp = self.request({
            "MessageType": "ReadPairRecord",
            "PairRecordID": udid,
            "ClientVersionString": "meridian-py",
            "ProgName": "meridian-py",
            "kLibUSBMuxVersion": 3,
        })
        if "PairRecordData" in resp:
            return plistlib.loads(resp["PairRecordData"])
        raise MuxError(f"no pair record for {udid}")

    def request(self, payload: dict, tag: int = 1) -> dict:
        s = self._connect()
        try:
            s.sendall(self._pack(tag, payload))
            return self._recv(s)
        finally:
            s.close()
