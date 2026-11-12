#!/usr/bin/env python3
"""PHY receive-polarity tests for the CH32V307 built-in 10M PHY (eth_bench).

  phy_tests.py HOST pn [--ms 3000]       # forced normal / forced reversed / automatic
  phy_tests.py HOST link [--modes 2 0 1]  # cable "replug" (PHY power-down) per polarity mode
  phy_tests.py HOST restart [--modes 2 0 1] [--secs 30]  # polarity + auto-negotiation restart

`pn` runs /pntest while sending UDP echo requests every 10 ms, and prints the
device's per-phase MAC counters plus echo replies per 0.5 s.
`link` runs /linktest/<mode> (0 normal, 1 reversed, 2 automatic): the link drops
for ~1 s, so the result is read back from /linkresult afterwards. If your OS
drops the interface address when the link goes down, re-enable it while this
runs (e.g. NetworkManager without autoconnect).
`restart` resets the board (`wlink reset`), sets the polarity mode with
/anpol/<mode> (which also restarts auto-negotiation) and polls /diag for
`--secs`. A mode that is wrong for the cable never answers again; the next
reset restores the board.
"""
import argparse, json, socket, subprocess, threading, time, urllib.request

ap = argparse.ArgumentParser()
ap.add_argument("host"); ap.add_argument("test", choices=["pn", "link", "restart"])
ap.add_argument("--ms", type=int, default=3000)
ap.add_argument("--modes", type=int, nargs="+", default=[2, 0, 1])
ap.add_argument("--secs", type=int, default=30)
a = ap.parse_args()
H = a.host


def get(p, t=3):
    return urllib.request.urlopen(f"http://{H}{p}", timeout=t).read().decode()


def echo_loop(stop, sent, got, t0):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(0.005)
    n = 0
    while not stop.is_set():
        try:
            s.sendto(b"E" + n.to_bytes(4, "big"), (H, 5001)); sent.append(time.time() - t0); n += 1
            while True:
                d, _ = s.recvfrom(64)
                if d[:1] == b"E":
                    got.append(time.time() - t0)
        except OSError:
            pass
        time.sleep(0.005)


if a.test == "pn":
    res = {}
    stop = threading.Event(); sent, got = [], []; t0 = time.time()
    th = threading.Thread(target=echo_loop, args=(stop, sent, got, t0)); th.start()
    try:
        res = json.loads(get(f"/pntest/{a.ms}", t=60))
    finally:
        time.sleep(0.5); stop.set(); th.join()
    for p in res.get("phases", []):
        print(p)
    for b in range(int(max(sent) / 0.5) + 1):
        lo, hi = b * 0.5, (b + 1) * 0.5
        print(f"{lo:5.1f}-{hi:5.1f} s: sent {sum(lo <= v < hi for v in sent):3d}, "
              f"replies {sum(lo <= v < hi for v in got):3d}")
elif a.test == "restart":
    NAMES = {0: "forced normal", 1: "forced reversed", 2: "automatic"}

    def wait_up(secs):
        t = time.time()
        while time.time() - t < secs:
            try:
                return json.loads(get("/diag", t=1))
            except Exception:
                time.sleep(0.5)
        return None

    for mode in a.modes:
        subprocess.run(["wlink", "reset"], capture_output=True, timeout=30)
        if wait_up(25) is None:
            raise SystemExit("device not reachable after reset")
        get(f"/anpol/{mode}")
        # The change is applied 200 ms after the answer; give auto-negotiation
        # time to finish before reading the PHY.
        time.sleep(3)
        d = wait_up(a.secs)
        if d is None:
            print(f"{NAMES[mode]:16s} no answer within {a.secs + 3} s")
        else:
            print(f"{NAMES[mode]:16s} link OK: mdix {d['phy_mdix']} "
                  f"bmsr {d['phy_bmsr']} anlpar {d['phy_anlpar']} crc_err {d['mmc_rx_crc_err']}")
    subprocess.run(["wlink", "reset"], capture_output=True, timeout=30)
else:
    for mode in a.modes:
        get(f"/linktest/{mode}")
        stop = threading.Event(); sent, got = [], []
        th = threading.Thread(target=echo_loop, args=(stop, sent, got, time.time())); th.start()
        time.sleep(14); stop.set(); th.join()
        r = None
        for _ in range(30):
            try:
                r = json.loads(get("/linkresult")); break
            except Exception:
                time.sleep(1)
        name = {0: "forced normal", 1: "forced reversed", 2: "automatic"}[mode]
        print(f"{name:16s} {r}  echo replies {len(got)}/{len(sent)}")
