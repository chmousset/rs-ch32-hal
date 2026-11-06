# CH32V307 Ethernet test app (`eth_bench`)

Test firmware and host tools for the `eth::v1` driver with the built-in 10M PHY. Used to find and
validate the fixes on this branch (on top of PR #231):

- RX FIFO overflow leaves the MAC reporting stale frame lengths (recovered by the driver now)
- bad RX descriptors delaying good frames, duplex taken from the PHY, no promiscuous mode

## Build and flash

```bash
cd examples/ch32v307
cargo build --release --bin eth_bench
wlink flash target/riscv32imfc-unknown-none-elf/release/eth_bench
```

`cargo run` also works, but the crate's runner keeps `wlink --watch-serial` attached, which stalls
the core for milliseconds: detach it for load tests. SDI prints are off by default (`LOG_EVENTS`,
`STATUS_PRINT`), except in the link test.

Network: `STATIC_IP` in `src/bin/eth_bench/main.rs` defaults to `192.168.1.50/24` for a direct
cable to the test PC (e.g. PC at `192.168.1.100/24`). Set it to `None` for DHCP (address printed
over SDI).

## Device endpoints

| Endpoint | Use |
|---|---|
| `GET /` | Small JS client showing stream statistics |
| `GET /ws` | WebSocket stream of binary frames (16 B header with sequence number, then `n` u16 ADC samples); text command `cfg <period_us> <n> [nagle]` |
| `GET /stats`, `GET /diag` | Counters; MAC/DMA/PHY registers, MMC counters |
| `GET /peek/<addr>`, `GET /poke/<addr>/<val>` | Raw 32-bit register read/write (hex, 0x4000_0000..0x6000_0000) |
| `GET /rxlog` | Last 32 frames handed to the stack: `len:tag` (tag = bytes 42..46, the UDP payload start) |
| `GET /pntest/<ms>` | PHY receive polarity: forced normal, forced reversed, automatic (from reversed), `<ms>` each |
| `GET /linktest/<0,1,2>`, `GET /linkresult` | Cable "replug" (PHY power-down 1 s) with polarity normal / reversed / automatic |
| UDP 5001 | `E...`: echoed. `tx <count> <len>`: sends datagrams. Anything else: counted (`udp_flood`) |

## Host tools (`eth_bench/`)

```bash
cd examples/ch32v307/eth_bench
python3 -m venv .venv && .venv/bin/pip install -r requirements.txt
export PY=.venv/bin/python
H=192.168.1.50
```

| Command | What it shows |
|---|---|
| `$PY bench.py $H --period 1000 --n 720 --nagle 0 --secs 10 --clients 3` | Streaming throughput, loss, jitter (`--sweep` for a table) |
| `$PY eth_len_probe.py $H --quick` | Sends WebSocket frames of many lengths; `healthy` or `LOST lengths [...]` |
| `$PY b_probe.py $H check` | Resets the board (`wlink reset`), then up to 6 × (3 clients + 3000 frames/s UDP flood for 5 s), length probe after each. `PASS` = never reaches the bad state |
| `$PY b_probe.py $H seq` | In the bad state: per-frame lengths from `/rxlog` for 4n / non-4n frame sequences |
| `$PY b_probe.py $H counters` | In the bad state: UDP delivery per frame size vs the MAC's good-frame counter |
| `$PY phy_tests.py $H pn` | Polarity phases with a 10 ms UDP echo timeline |
| `$PY phy_tests.py $H link` | Replug per polarity mode (the link drops; result read back afterwards) |

## Expected results (direct link, 144 MHz)

**RX FIFO overflow.** With PR #231's driver alone, `b_probe.py check` fails after the first load
run. `seq` shows that frames whose length (incl. FCS) is a multiple of 4 carry the previous frame's
length; the data is correct, and nothing but a MAC/DMA soft reset clears it:

```
sent (len incl FCS:tag): 148:05 152:06 156:07
log  (len w/o FCS:tag):  60:05  144:06 148:07     (expected 144, 148, 152)
sent: 149:0a 148:0b 151:0c  ->  log: 145:0a 145:0b 147:0c   (only the 4n frame is wrong)
```

The chip does not set `DMASR.ROS` for these overflows; they only show in `DMAMFBOCR`. With this
branch, `check` passes (12/12 load runs) and the driver keeps `DMAMFBOCR` at 0 (it reads it).

Note: in this example (embassy-net 0.9) the WebSocket load alone did not overflow the FIFO (0 in
3 runs); the UDP flood makes the trigger reliable.

**Polarity**, normal cable (`phy_tests.py pn`): forced reversed → link stays up, every frame fails
its CRC, no echo replies; switching to automatic restores reception at once (0 CRC errors).
`link`: after a replug, forced reversed never links (`BMSR` 0x1809, `ANLPAR` 0); automatic links in
~0.9 s.

With a cable that reverses the polarity of both pairs, the results mirror: forced normal fails
(0 good frames, CRC errors; never links after a replug), forced reversed and automatic work
(link in ~0.9 s, 0 CRC errors). The link partner (an RTL8153 USB adapter) corrects its own receive
pair. Run each `link` mode from a fresh boot: a failed case leaves the PHY in a state that affects
the next one.

## Notes

- RAM: 4 RX/4 TX descriptors (#231's default, also makes overflows easier to trigger). The UDP
  buffers are kept small (512 B) to leave ~2 kB of stack headroom (`/stats` `stack_free`).
- `/poke` and the PHY tests change hardware state; reset the board after experimenting.
