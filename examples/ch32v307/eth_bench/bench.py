#!/usr/bin/env python3
"""Desktop benchmark client for the CH32 WebSocket server.

Usage:
  bench.py HOST [--period US] [--n SAMPLES] [--secs S] [--clients K]
  bench.py HOST --sweep           # grid of period x n, prints a markdown table

Metrics per run (per client):
  fps      received frames / s
  goodput  WS payload bytes / s (application data incl. 16 B frame header)
  wire     estimated TCP payload bytes / s (payload + 2..4 B WS header)
  loss     frames missing from the seq counter (gaps) / frames expected
  jitter   inter-arrival time p50 / p99 / max (ms)
"""
import argparse
import asyncio
import json
import statistics
import struct
import time

import urllib.request

import websockets

IDLE_BASELINE = 376_000  # V307 idle_loops/s with no client; recalibrate with --idle-baseline
IDLE_BASELINE_CUR = [IDLE_BASELINE]  # overridable with --idle-baseline


def dev_stats(url):
    host = url.split("//")[1].split("/")[0]
    try:
        return json.load(urllib.request.urlopen(f"http://{host}/stats", timeout=2))
    except Exception:
        return None

HDR = struct.Struct("<BBHIII")  # magic, kind, n, seq, t_us, lag


async def run_one(url, period, n, secs, warmup=0.5, send_cfg=True, nagle=1, measure_cpu=True):
    async with websockets.connect(url, max_size=None, compression=None) as ws:
        cfg = json.loads(await ws.recv())
        if send_cfg:
            await ws.send(f"cfg {period} {n} {nagle}")
            # Drain until the cfg ack arrives.
            while True:
                m = await ws.recv()
                if isinstance(m, str):
                    cfg = json.loads(m)
                    break
        # Warmup: let the channel settle after the cfg change.
        t_end = time.perf_counter() + warmup
        while time.perf_counter() < t_end:
            await ws.recv()

        frames = 0
        payload = 0
        wire = 0
        first_seq = last_seq = None
        gaps = 0
        lag0 = lag1 = None
        arrivals = []
        s0 = await asyncio.to_thread(dev_stats, url) if measure_cpu else None
        t0 = time.perf_counter()
        t_end = t0 + secs
        while True:
            m = await ws.recv()
            now = time.perf_counter()
            if isinstance(m, str):
                continue
            magic, kind, cnt, seq, t_us, lag = HDR.unpack_from(m)
            assert magic == 0x57 and kind == 1 and len(m) == 16 + 2 * cnt
            if first_seq is None:
                first_seq = seq
                lag0 = lag
            else:
                d = (seq - last_seq) & 0xFFFFFFFF
                gaps += d - 1
            last_seq = seq
            lag1 = lag
            frames += 1
            payload += len(m)
            wire += len(m) + (2 if len(m) < 126 else 4)
            arrivals.append(now)
            if now >= t_end:
                break
        s1 = await asyncio.to_thread(dev_stats, url) if measure_cpu else None
        t1 = time.perf_counter()
        idle = None
        if s0 and s1 and "idle_loops" in s0:
            rate = ((s1["idle_loops"] - s0["idle_loops"]) & 0xFFFFFFFF) / ((s1["uptime_ms"] - s0["uptime_ms"]) / 1e3)
            idle = 100 * rate / IDLE_BASELINE_CUR[0]
        dt = arrivals[-1] - arrivals[0] if len(arrivals) > 1 else secs
        ia = sorted((b - a) * 1e3 for a, b in zip(arrivals, arrivals[1:]))
        expected = frames + gaps
        return {
            "period_us": cfg["period_us"],
            "nagle": cfg.get("nagle"),
            "n": cfg["n"],
            "frames": frames,
            "fps": (frames - 1) / dt,
            "goodput": payload / dt,
            "wire": wire / dt,
            "loss": gaps / expected if expected else 0.0,
            "srv_lag": (lag1 - lag0) if lag0 is not None else 0,
            "ia_p50": ia[len(ia) // 2] if ia else 0,
            "ia_p99": ia[int(len(ia) * 0.99)] if ia else 0,
            "ia_max": ia[-1] if ia else 0,
            "overruns": cfg.get("overruns"),
            "cpu_idle": idle,
        }


def fmt(r, k=None):
    who = f"[c{k}] " if k is not None else ""
    return (
        f"{who}period={r['period_us']}us n={r['n']} nagle={r['nagle']}: {r['fps']:.0f} fps, "
        f"goodput {r['goodput']/1e3:.1f} kB/s ({r['goodput']*8/1e6:.2f} Mbit/s), "
        f"loss {100*r['loss']:.2f}% (srv lag {r['srv_lag']}), "
        f"ia p50/p99/max {r['ia_p50']:.2f}/{r['ia_p99']:.2f}/{r['ia_max']:.1f} ms"
        + (f", cpu idle ~{r['cpu_idle']:.0f}%" if r.get("cpu_idle") is not None else "")
    )


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("host")
    ap.add_argument("--period", type=int, default=10000)
    ap.add_argument("--n", type=int, default=64)
    ap.add_argument("--secs", type=float, default=5)
    ap.add_argument("--clients", type=int, default=1)
    ap.add_argument("--nagle", type=int, default=1)
    ap.add_argument("--idle-baseline", type=float, default=IDLE_BASELINE)
    ap.add_argument("--sweep", action="store_true")
    ap.add_argument("--sizes", default="8,64,256,720")
    ap.add_argument("--periods", default="10000,2000,1000,500,250,100")
    a = ap.parse_args()
    IDLE_BASELINE_CUR[0] = a.idle_baseline
    url = f"ws://{a.host}/ws"

    if not a.sweep:
        rs = await asyncio.gather(
            *(run_one(url, a.period, a.n, a.secs, send_cfg=(i == 0), warmup=0.5 + 0.3 * (i == 0), nagle=a.nagle,
                      # /stats needs a fourth TCP slot when all three WS slots
                      # are occupied. Skip it rather than perturbing the run.
                      measure_cpu=(i == 0 and a.clients < 3))
              for i in range(a.clients))
        )
        for i, r in enumerate(rs):
            # Other clients saw the cfg from before client 0 changed it.
            r.update({k: rs[0][k] for k in ("period_us", "n", "nagle")})
            print(fmt(r, i if a.clients > 1 else None))
        if a.clients > 1:
            tot = sum(r["goodput"] for r in rs)
            print(f"aggregate goodput {tot/1e3:.1f} kB/s ({tot*8/1e6:.2f} Mbit/s)")
        return

    print("| nagle | period µs | n | target fps | fps | goodput kB/s | Mbit/s | loss % |"
          " ia p99 ms | ia max ms | CPU idle % |")
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    for n in map(int, a.sizes.split(",")):
        for period in map(int, a.periods.split(",")):
            r = await run_one(url, period, n, a.secs, nagle=a.nagle)
            print(f"| {a.nagle} | {period} | {n} | {1e6/period:.0f} | {r['fps']:.0f} | "
                  f"{r['goodput']/1e3:.1f} | {r['goodput']*8/1e6:.2f} | "
                  f"{100*r['loss']:.2f} | {r['ia_p99']:.2f} | {r['ia_max']:.1f} | "
                  + (f"{r['cpu_idle']:.0f} |" if r['cpu_idle'] is not None else "n/a |"), flush=True)


if __name__ == "__main__":
    asyncio.run(main())
