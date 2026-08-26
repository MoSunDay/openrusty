#!/usr/bin/env python3
"""Minimal HTTP/1.1 keep-alive load generator (stdlib asyncio only).

Emits a single JSON line on stdout; diagnostics go to stderr. C workers
each hold one persistent connection and issue back-to-back GETs until the
deadline; on any socket/parse/timeout error the worker counts it and
reconnects. Latency covers write -> full response body read.
"""
import argparse
import asyncio
import json
import socket
import sys
import time

REQ_HEADERS = (
    "User-Agent: openrusty-bench/1\r\n"
    "Accept: */*\r\n"
    "X-Bench-Probe: 1\r\n"
)
READ_LIMIT = 1 << 16


def build_request(path, host, extra_headers):
    pad = "".join(
        f"X-Bench-Pad{i:02}: 0123456789abcdef{i:02}\r\n" for i in range(extra_headers)
    )
    return (f"GET {path} HTTP/1.1\r\nHost: {host}\r\n{REQ_HEADERS}{pad}\r\n").encode()


def parse_head(head):
    """-> (status, headers-lower-dict); raises ValueError on garbage."""
    lines = head.decode("latin-1").split("\r\n")
    parts = lines[0].split(" ")
    if len(parts) < 2 or not parts[1].isdigit():
        raise ValueError(f"bad status line: {lines[0]!r}")
    headers = {}
    for line in lines[1:]:
        if not line:
            continue
        name, _, value = line.partition(":")
        headers[name.strip().lower()] = value.strip()
    return int(parts[1]), headers


async def read_body(reader, headers):
    if headers.get("transfer-encoding", "").lower() == "chunked":
        while True:
            size = int((await reader.readuntil(b"\r\n")).split(b";")[0], 16)
            await reader.readexactly(size)
            await reader.readexactly(2)
            if size == 0:
                break
        while True:  # trailers
            line = await reader.readuntil(b"\r\n")
            if line == b"\r\n":
                break
        return
    length = headers.get("content-length")
    if length is None:
        raise ValueError("no content-length")
    await reader.readexactly(int(length))


async def one_exchange(reader, writer, request):
    """-> (status, peer_advertised_close)."""
    writer.write(request)
    await writer.drain()
    head = await reader.readuntil(b"\r\n\r\n")
    status, headers = parse_head(head[:-4])
    await read_body(reader, headers)
    return status, headers.get("connection", "").lower() == "close"


async def drop(writer):
    if writer is None:
        return
    writer.close()
    try:
        await writer.wait_closed()
    except OSError:
        pass


async def reconnect(addr, deadline):
    """-> (reader, writer) or (None, None) at deadline."""
    while time.monotonic() < deadline:
        try:
            return await connect(addr)
        except OSError:
            await asyncio.sleep(0.02)
    return None, None


async def connect(addr):
    host, port = addr
    reader, writer = await asyncio.open_connection(host, port, limit=READ_LIMIT)
    sock = writer.get_extra_info("socket")
    if sock is not None:
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return reader, writer


async def worker(index, addrs, request, deadline, timeout, conn):
    """Returns (ok_count, err_count, latencies_ms) from one warm connection.

    A close the peer advertised in its last response is honored without
    counting an error (e.g. nginx keepalive_requests); only failures while
    a request is outstanding count.
    """
    addr = addrs[index % len(addrs)]
    reader, writer = conn
    ok, errs, lats = 0, 0, []
    while time.monotonic() < deadline:
        if writer is None:
            reader, writer = await reconnect(addr, deadline)
            if writer is None:
                errs += 1
                break
        started = time.monotonic()
        try:
            status, close = await asyncio.wait_for(
                one_exchange(reader, writer, request), timeout
            )
            if status != 200:
                raise ValueError(f"status {status}")
            ok += 1
            lats.append((time.monotonic() - started) * 1000.0)
            if close:
                await drop(writer)
                reader = writer = None
        except (OSError, ValueError, asyncio.IncompleteReadError,
                asyncio.LimitOverrunError, asyncio.TimeoutError) as exc:
            errs += 1
            note_error(exc)
            await drop(writer)
            reader = writer = None
    await drop(writer)
    return ok, errs, lats


ERROR_NOTES = {}


def note_error(exc):
    kind = type(exc).__name__
    detail = str(exc).split("\r")[0][:120]
    key = f"{kind}: {detail}"
    ERROR_NOTES[key] = ERROR_NOTES.get(key, 0) + 1


def percentile(sorted_lats, fraction):
    if not sorted_lats:
        return 0.0
    idx = min(len(sorted_lats) - 1, int(round(fraction * (len(sorted_lats) - 1))))
    return sorted_lats[idx]


def summarize(results, wall):
    count = sum(r[0] for r in results)
    errs = sum(r[1] for r in results)
    lats = sorted(lat for r in results for lat in r[2])
    return {
        "count": count,
        "err": errs,
        "rps": round(count / wall, 1) if wall > 0 else 0.0,
        "p50_ms": round(percentile(lats, 0.50), 3),
        "p90_ms": round(percentile(lats, 0.90), 3),
        "p99_ms": round(percentile(lats, 0.99), 3),
        "max_ms": round(lats[-1], 3) if lats else 0.0,
        "avg_ms": round(sum(lats) / len(lats), 3) if lats else 0.0,
    }


async def run(addrs, path, concurrency, duration, timeout, host_header, extra_headers):
    request = build_request(path, host_header, extra_headers)
    connected = []
    start = asyncio.Event()
    deadline_holder = [0.0]

    async def guarded_worker(i):
        while True:
            try:
                reader, writer = await connect(addrs[i % len(addrs)])
            except OSError:
                await asyncio.sleep(0.05)
                continue
            connected.append(i)
            await start.wait()
            if deadline_holder[0] < time.monotonic():  # aborted ramp
                await drop(writer)
                return 0, 0, []
            return await worker(i, addrs, request, deadline_holder[0],
                                timeout, (reader, writer))

    tasks = [asyncio.create_task(guarded_worker(i)) for i in range(concurrency)]
    ramp_deadline = time.monotonic() + 60.0
    while len(connected) < concurrency and time.monotonic() < ramp_deadline:
        await asyncio.sleep(0.05)
    if len(connected) < concurrency:
        deadline_holder[0] = time.monotonic()  # unblock ramp, then fail
        start.set()
        await asyncio.gather(*tasks, return_exceptions=True)
        print(f"ramp failed: {len(connected)}/{concurrency} connected",
              file=sys.stderr)
        sys.exit(2)
    deadline_holder[0] = time.monotonic() + duration
    started = time.monotonic()
    start.set()
    results = await asyncio.gather(*tasks)
    wall = time.monotonic() - started
    return summarize(results, wall), wall


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", action="append", required=True,
                        help="host:port, repeatable (round-robin per worker)")
    parser.add_argument("--path", default="/")
    parser.add_argument("--concurrency", type=int, default=100)
    parser.add_argument("--duration", type=float, default=10.0)
    parser.add_argument("--timeout", type=float, default=10.0)
    parser.add_argument("--host-header", default="bench.openrusty.local")
    parser.add_argument("--extra-headers", type=int, default=0,
                        help="append N synthetic ~30B headers (header-cost probes)")
    args = parser.parse_args()

    addrs = []
    for target in args.target:
        host, _, port = target.rpartition(":")
        addrs.append((host, int(port)))

    stats, wall = asyncio.run(
        run(addrs, args.path, args.concurrency, args.duration,
            args.timeout, args.host_header, args.extra_headers)
    )
    stats.update(
        wall_s=round(wall, 3), c=args.concurrency,
        path=args.path, targets=",".join(args.target),
    )
    for note, n in sorted(ERROR_NOTES.items(), key=lambda kv: -kv[1])[:5]:
        print(f"err-detail {n}x {note}", file=sys.stderr)
    print(json.dumps(stats, ensure_ascii=False))


if __name__ == "__main__":
    main()
