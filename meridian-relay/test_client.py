#!/usr/bin/env python3
"""Test meridian-relay daemon with proper usbmuxd protocol."""
import socket
import struct
import plistlib
import time
import sys

SOCKET_PATH = "/tmp/meridian-relay-usbmuxd.sock"

def send_packet(sock, plist_data, tag=0):
    """Send a usbmuxd protocol packet."""
    payload = plistlib.dumps(plist_data, fmt=plistlib.FMT_XML)
    header = struct.pack('<IIII', len(payload) + 16, 1, 8, tag)
    sock.sendall(header + payload)

def recv_packet(sock, timeout=5):
    """Receive a usbmuxd protocol packet."""
    sock.settimeout(timeout)
    try:
        header = sock.recv(16)
        if len(header) < 16:
            return None, None
        size, ver, msg, tag = struct.unpack('<IIII', header)
        body = b''
        remaining = size - 16
        while remaining > 0:
            chunk = sock.recv(remaining)
            if not chunk:
                break
            body += chunk
            remaining -= len(chunk)
        try:
            data = plistlib.loads(body)
        except Exception as e:
            print(f"  plist parse error: {e}")
            print(f"  raw body ({len(body)} bytes): {body}")
            return None, None
        return data, tag
    except socket.timeout:
        return None, None
    except Exception as e:
        print(f"  recv error: {e}")
        return None, None

def main():
    print("=== Meridian Relay Test Client ===\n")

    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        s.connect(SOCKET_PATH)
        print(f"1. Connected to {SOCKET_PATH}")
    except Exception as e:
        print(f"Failed to connect: {e}")
        sys.exit(1)

    # ListDevices
    print("\n2. Sending ListDevices...")
    send_packet(s, {'MessageType': 'ListDevices'}, tag=1)
    resp, tag = recv_packet(s)
    if resp:
        devices = resp.get('DeviceList', [])
        print(f"   Found {len(devices)} device(s)")
        for dev in devices:
            props = dev.get('Properties', {})
            print(f"   - DeviceID={dev.get('DeviceID')} "
                  f"Serial={props.get('SerialNumber', '?')} "
                  f"PID=0x{props.get('ProductID', 0):04X} "
                  f"Conn={props.get('ConnectionType', '?')}")
        
        if not devices:
            print("   ERROR: No devices found!")
            s.close()
            return

        device_id = devices[0]['DeviceID']
    else:
        print("   No response!")
        s.close()
        return

    # Connect to lockdown (port 62078)
    print(f"\n3. Connecting to device {device_id} port 62078...")
    send_packet(s, {
        'MessageType': 'Connect',
        'DeviceID': device_id,
        'PortNumber': 62078,
    }, tag=2)
    resp, tag = recv_packet(s)
    if resp:
        number = resp.get('Number', -1)
        msg = resp.get('MessageType', '?')
        print(f"   Response: MessageType={msg} Number={number}")
        if number != 0:
            print(f"   ERROR: Connect failed with code {number}")
            s.close()
            return
    else:
        print("   No response!")
        s.close()
        return

    print("   Connected! Entering lockdown protocol...")

    # Now we're connected to lockdown on port 62078
    # Send StartSession
    print("\n4. Sending Lockdown StartSession...")
    session_req = {
        'Label': 'com.apple.mobile.lockdown',
        'Request': 'StartSession',
        'ProtocolVersion': '2',
        'HostID': 'test-host-meridian',
        'SystemBUID': 'test-system-buid-meridian',
    }
    
    # Lockdown uses binary plist
    payload = plistlib.dumps(session_req, fmt=plistlib.FMT_BINARY)
    s.sendall(payload)
    print(f"   Sent {len(payload)} bytes")

    # Read response
    print("\n5. Waiting for Lockdown response...")
    s.settimeout(10)
    try:
        resp_data = s.recv(65536)
        if resp_data:
            print(f"   Received {len(resp_data)} bytes")
            try:
                resp_plist = plistlib.loads(resp_data)
                print(f"   Lockdown response: {dict(resp_plist)}")
            except:
                print(f"   Raw hex: {resp_data[:100].hex()}")
                print(f"   Raw text: {resp_data[:200]}")
        else:
            print("   Empty response (connection closed)")
    except socket.timeout:
        print("   Timeout waiting for response")
    except Exception as e:
        print(f"   Error: {e}")

    s.close()
    print("\n6. Done!")

if __name__ == '__main__':
    main()
