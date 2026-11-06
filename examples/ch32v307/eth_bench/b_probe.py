#!/usr/bin/env python3
"""Probes for the RX FIFO overflow problem of the CH32V307 MAC: after an overflow,
frames whose length (incl. FCS) is a multiple of 4 get the previous frame's
length. Needs eth_bench (UDP port 5001, /stats, /rxlog, /peek).

  b_probe.py HOST check      # overload, then length probe: PASS = never reaches the bad state
  b_probe.py HOST counters   # bad state: UDP frames per size, delivered vs sent
  b_probe.py HOST single     # bad state: a 4n frame alone vs followed by another frame
  b_probe.py HOST seq        # bad state: per-frame lengths from /rxlog (shows the stale length)

The device is reset (wlink) and loaded with 3 WebSocket clients plus a UDP flood first. With the
overflow recovery in the driver, the bad state is never reached; the counters/
single/seq modes are for a driver without it (e.g. PR #231 alone).
"""
import argparse, json, socket, subprocess, sys, threading, time, urllib.request

H = None
PAYLOADS = (34, 35, 38, 42, 46, 54, 74, 102, 130, 154, 210, 254, 510, 1002, 1402)
import os
TOOLS = os.path.dirname(os.path.abspath(__file__))
PY = os.environ.get("PY", sys.executable)

# Register addresses (CH32V307 ETH: MAC 0x4002_8000, DMA 0x4002_9000).
MACCR, MIIAR, MIIDR = 0x40028000, 0x40028010, 0x40028014
MMC_CRC, MMC_ALIGN, MMC_UCAST = 0x40028194, 0x40028198, 0x400281C4
DMABMR, DMASR, DMAOMR, DMAMFBOCR = 0x40029000, 0x40029014, 0x40029018, 0x40029020


def get(path, t=3, tries=4):
    # In the bad state, requests whose frame length is 4n get truncated: retry.
    for i in range(tries):
        try:
            return urllib.request.urlopen(f"http://{H}{path}", timeout=t).read().decode()
        except Exception:
            if i == tries - 1:
                raise
            time.sleep(0.3)


def peek(a):
    return int(get(f"/peek/{a:08x}"), 16)


def poke(a, v):
    return int(get(f"/poke/{a:08x}/{v:08x}"), 16)


def stats():
    return json.loads(get("/stats"))


def sh(cmd, t=120):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=t).stdout


def wait_up(secs=25):
    t = time.time()
    while time.time() - t < secs:
        try:
            stats()
            return True
        except Exception:
            time.sleep(0.5)
    return False


def lenprobe():
    out = sh(f"{PY} {TOOLS}/eth_len_probe.py {H} --quick", 120).strip().splitlines()
    return out[-1].replace("RESULT: ", "") if out else "?"


def flood(stop, pps=3000):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    n, t0 = 0, time.perf_counter()
    while not stop.is_set():
        s.sendto(b"F" * 18, (H, 5001))
        n += 1
        d = t0 + n / pps - time.perf_counter()
        if d > 0:
            time.sleep(d)


def go_bad(max_tries=6, exit_if_not=True):
    sh("wlink reset", 30)
    if not wait_up():
        sys.exit("device not reachable after reset")
    for i in range(max_tries):
        # 3 streaming clients (~9.5 Mbit/s out, ACKs in) plus a 3000 frames/s flood
        # of small UDP frames: the RX path falls behind and the RX FIFO overflows.
        stop = threading.Event()
        fl = threading.Thread(target=flood, args=(stop,)); fl.start()
        sh(f"{PY} {TOOLS}/bench.py {H} --period 1000 --n 720 --nagle 0 --secs 5 --clients 3", 60)
        stop.set(); fl.join()
        r = lenprobe()
        if r.startswith("LOST"):
            st = stats()
            print(f"bad state after {i + 1} load run(s): {r}")
            return True
    if exit_if_not:
        sys.exit(f"could not reach bad state in {max_tries} load runs")
    return False


def udp_burst(payload, n=200, gap=0.003):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    data = b"F" * payload
    for _ in range(n):
        s.sendto(data, (H, 5001))
        time.sleep(gap)
    time.sleep(0.3)


def counters():
    def snap():
        st = stats()
        return {"app": st["udp_flood"], "mmc_ucast": peek(MMC_UCAST), "crc": peek(MMC_CRC),
                "align": peek(MMC_ALIGN)}
    # Frame length incl. FCS = 14 + 20 + 8 + payload + 4 = payload + 46.
    for p in PAYLOADS:
        a = snap()
        udp_burst(p)
        b = snap()
        d = {k: b[k] - a[k] for k in a}
        # Each /stats call itself adds received frames (TCP), counted in mmc_ucast.
        print(f"payload {p} (frame {p + 46}, %4={(p + 46) % 4}): sent 200 -> app {d['app']}, "
              f"MMC good ucast +{d['mmc_ucast']}, crc +{d['crc']}, align +{d['align']}")
    print(f"DMASR {peek(DMASR):08x} DMAOMR {peek(DMAOMR):08x} MACCR {peek(MACCR):08x}")


def single():
    """One 4n frame alone, then followers: is it stuck until the next frame?"""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    app = lambda: stats()["udp_flood"]
    # stats() itself is TCP traffic to the device: read before/after with no UDP in between.
    for trial in range(3):
        a = app()
        s.sendto(b"F" * 102, (H, 5001))        # frame 148 (4n)
        time.sleep(1.0)
        b = app()
        s.sendto(b"F" * 103, (H, 5001))        # frame 149 (4n+1) as follower
        time.sleep(0.5)
        c = app()
        print(f"trial {trial}: 4n alone -> +{b - a} after 1 s (the /stats requests in between are TCP frames); "
              f"after a 4n+1 follower -> +{c - b}")
    a = app()
    s.sendto(b"F" * 102, (H, 5001))
    s.sendto(b"F" * 102, (H, 5001))
    time.sleep(1.0)
    b = app()
    print(f"two 4n back to back -> +{b - a}")


def udp(tag, payload_len, s=[None]):
    if s[0] is None:
        s[0] = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s[0].sendto(tag.to_bytes(4, "big") + b"F" * (payload_len - 4), (H, 5001))


def rxlog():
    # Frames tagged 0xA5...... are ours; the rest is TCP (the /rxlog request itself etc.).
    out = []
    for e in get("/rxlog").split():
        l, t = e.split(":")
        t = int(t, 16)
        if t >> 24 == 0xA5:  # only our frames
            out.append(f"{l}:{t & 0xFF:02x}")
    return " ".join(out)


def seq():
    """Which frames come out, and with which length, for 4n / non-4n sequences."""
    cases = [
        ("A(4n) alone, then B(4n+1) 0.2 s later", [(1, 102, 0.2), (2, 103, 0)]),
        ("A(4n) B(4n+1) back to back", [(3, 102, 0), (4, 103, 0)]),
        ("A(4n) B(4n) C(4n) back to back", [(5, 102, 0), (6, 106, 0), (7, 110, 0)]),
        ("A(4n) B(4n) 20 ms apart", [(8, 102, 0.02), (9, 106, 0)]),
        ("A(4n+1) B(4n) C(4n+1) back to back", [(10, 103, 0), (11, 102, 0), (12, 105, 0)]),
    ]
    for name, frames in cases:
        get("/rxlog")  # flush older entries out of view
        for tag, plen, gap in frames:
            udp(0xA5000000 | tag, plen)
            if gap:
                time.sleep(gap)
        time.sleep(0.5)
        sent = " ".join(f"{p + 46}:{t:02x}" for t, p, _ in frames)
        print(f"{name}\n  sent (frame len incl FCS:tag): {sent}\n  log  (len w/o FCS:tag): {rxlog()}")


def main():
    global H
    ap = argparse.ArgumentParser()
    ap.add_argument("host")
    ap.add_argument("mode", choices=["check", "counters", "single", "seq"])
    a = ap.parse_args()
    H = a.host
    if a.mode == "check":
        bad = go_bad(exit_if_not=False)
        print("FAIL: bad state reached" if bad else "PASS: still healthy after 6 overload runs")
        sys.exit(1 if bad else 0)
    go_bad()
    if a.mode == "counters":
        counters()
    elif a.mode == "single":
        single()
    elif a.mode == "seq":
        seq()



if __name__ == "__main__":
    main()
