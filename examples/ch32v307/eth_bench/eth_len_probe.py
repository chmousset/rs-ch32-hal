#!/usr/bin/env python3
"""Detect the CH32V307 10M-MAC "length % 4" RX corruption.

For each WebSocket text payload length, open a fresh connection, send one
message, and check via GET /stats (ws_rx_bytes) whether the device received it.
Each probe uses its own connection, because a lost segment blocks the TCP
stream behind it (retransmits are lost too).

  eth_len_probe.py HOST [--lengths 1-40] [--quick]

--quick tests lengths 4,5,8,12 only. On a healthy device all pass. In the bad
state, lengths 4/8/12 (TCP payload 10/14/18, frame length % 4 == 0) fail and 5
passes.
"""
import argparse
import asyncio
import json
import urllib.request

import websockets


def stats(host):
    return json.load(urllib.request.urlopen(f"http://{host}/stats", timeout=3))


async def probe(host, length):
    # Pad a valid command with trailing spaces (the firmware ignores them).
    base = "cfg 1000 64 0"
    msg = (base + " " * 300)[:length] if length >= len(base) else base[:length]
    async with websockets.connect(f"ws://{host}/ws", ping_interval=None, compression=None,
                                  max_size=None, close_timeout=1) as ws:
        async def drain():
            while True:
                await ws.recv()
        t = asyncio.create_task(drain())
        s0 = await asyncio.to_thread(stats, host)
        await ws.send(msg)
        await asyncio.sleep(0.6)
        s1 = await asyncio.to_thread(stats, host)
        t.cancel()
    return s1["ws_rx_bytes"] - s0["ws_rx_bytes"]


def parse_lengths(spec):
    out = []
    for part in spec.split(","):
        a, _, b = part.partition("-")
        out += list(range(int(a), int(b or a) + 1))
    return out


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("host")
    ap.add_argument("--lengths", default="1-40")
    ap.add_argument("--quick", action="store_true")
    a = ap.parse_args()
    lengths = [4, 5, 8, 12] if a.quick else parse_lengths(a.lengths)
    bad = []
    for n in lengths:
        got = await probe(a.host, n)
        ws_frame = n + 6  # 2 B header + 4 B mask (client frames, n < 126)
        ok = got == ws_frame
        if not ok:
            bad.append(n)
        print(f"payload {n:3} -> TCP payload {ws_frame:3}: {'ok' if ok else 'LOST'}", flush=True)
    print("RESULT:", "healthy" if not bad else f"LOST lengths {bad}")


if __name__ == "__main__":
    asyncio.run(main())
