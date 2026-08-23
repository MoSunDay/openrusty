#!/usr/bin/env python3
"""Minimal raw-socket WebSocket client (no third-party deps).

Usage: ws_client.py HOST PORT PATH MESSAGE
Performs an HTTP/1.1 upgrade handshake, sends MESSAGE as a masked text
frame, waits for one echo frame, verifies the payload, then closes.
Exits 0 on success.
"""
import base64
import os
import socket
import struct
import sys


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("connection closed early")
        buf += chunk
    return buf


def read_response_headers(sock):
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(1024)
        if not chunk:
            raise ConnectionError("connection closed during handshake")
        buf += chunk
    head, rest = buf.split(b"\r\n\r\n", 1)
    return head.decode("latin1"), rest


def send_frame(sock, opcode, payload, mask=True):
    header = bytes([0x80 | opcode])
    length = len(payload)
    mask_bit = 0x80 if mask else 0
    if length < 126:
        header += bytes([mask_bit | length])
    elif length < 65536:
        header += bytes([mask_bit | 126]) + struct.pack(">H", length)
    else:
        header += bytes([mask_bit | 127]) + struct.pack(">Q", length)
    if mask:
        key = os.urandom(4)
        header += key
        payload = bytes(b ^ key[i % 4] for i, b in enumerate(payload))
    sock.sendall(header + payload)


def read_frame(sock):
    head = recv_exact(sock, 2)
    opcode = head[0] & 0x0F
    masked = bool(head[1] & 0x80)
    length = head[1] & 0x7F
    if length == 126:
        length = struct.unpack(">H", recv_exact(sock, 2))[0]
    elif length == 127:
        length = struct.unpack(">Q", recv_exact(sock, 8))[0]
    key = recv_exact(sock, 4) if masked else b""
    payload = recv_exact(sock, length)
    if masked:
        payload = bytes(b ^ key[i % 4] for i, b in enumerate(payload))
    return opcode, payload


def main():
    host, port, path, message = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
    with socket.create_connection((host, port), timeout=10) as sock:
        key = base64.b64encode(os.urandom(16)).decode()
        request = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        )
        sock.sendall(request.encode())
        head, _rest = read_response_headers(sock)
        status = head.split(" ", 2)[1]
        if status != "101":
            print(f"handshake failed: {head.splitlines()[0]}")
            return 1
        send_frame(sock, 0x1, message.encode())
        opcode, payload = read_frame(sock)
        if opcode != 0x1 or payload != message.encode():
            print(f"bad echo: opcode={opcode} payload={payload!r}")
            return 1
        send_frame(sock, 0x8, b"")  # close
        print("WS_ECHO_OK")
        return 0


if __name__ == "__main__":
    sys.exit(main())
