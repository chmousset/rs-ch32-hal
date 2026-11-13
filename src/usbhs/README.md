# USBHS device driver: maintainer notes

These notes are for anyone changing `mod.rs`, `endpoint.rs` or `control.rs`, or chasing a bug
in them. Application-facing limits and advice are in the module docs (`mod.rs`, rendered by
rustdoc).

The reference manual (CH32V30x RM V2.3, chapter 22, USBHS) describes the
registers but not how they behave around a transaction. Everything under "Measured" below
was established on a CH32V307 at full and high speed:
- bare-metal register-level test firmware (no USB stack), driven by a libusb host script;
- wire captures at full speed with a logic analyzer on D+/D-;
- a trace ring inside the driver, frozen on the first error a sequence-checked stream detects.

Other parts with this controller (CH32V305, CH32V317, CH32V467, CH32H417, CH569...) were not
measured.

Contents:
1. [Measured controller behaviour](#1-measured-controller-behaviour)
2. [Silicon: lost RX auto-toggle flip](#2-silicon-lost-rx-auto-toggle-flip)
3. [The interrupt handler, step by step](#3-the-interrupt-handler-step-by-step)
4. [OUT acceptance](#4-out-acceptance)
5. [EP0](#5-ep0)
6. [Rules for changing the code](#6-rules-for-changing-the-code)
7. [Debugging](#7-debugging)
8. [Known gaps](#8-known-gaps)

## 1. Measured controller behaviour

Register names follow the PAC: `INT_FG.TRANSFER`, `INT_ST` (`endp`, `token`, `tog_ok`, `is_nak`),
`RX_LEN`, `MIS_ST.SIE_FREE`, `CTRL.INT_BUSY`, and per endpoint `RX_CTRL` / `TX_CTRL` (`RES`,
`TOG`, `TOG_AUTO`) and `T_LEN`.

### `INT_BUSY`: one transaction per flag

With `INT_BUSY` set (the driver sets it), the controller NAKs **every** token while
`TRANSFER` is pending:
- every endpoint and both directions, including control data and status stages;
- no DMA write happens for a NAKed OUT;
- a SETUP is still accepted (`SETUP_ACT`), with its 8 bytes written to EP0's buffer.

Isochronous transactions ignore it: they complete and rewrite `INT_ST` / `RX_LEN` while the flag
is pending. That is why the iso paths in the handler look different.

After the release, the host's next packet to an endpoint left at `RES` = ACK arrives within
2-5 us at high speed (back-to-back 512-byte bulk OUTs). It lands in the same buffer.

### Which status survives, and for how long

While `TRANSFER` is pending (non-iso traffic only):

| Field | Behaviour |
| --- | --- |
| `INT_ST.endp`, `INT_ST.token` | Latched: describe the transaction that raised the flag. SOF, SETUP and NAKed tokens don't change them. |
| `RX_LEN` | Latched likewise. |
| `INT_ST.tog_ok` | **Live.** Every OUT NAKed meanwhile, to *any* endpoint, rewrites it with that packet's own toggle match. |
| `INT_ST.is_nak` | **Live.** Set by any NAK, cleared by the next successful transaction. |

Timeline with the flag left pending (EP1, back-to-back bulk OUT):

| | High speed | Full speed |
| --- | --- | --- |
| `IS_NAK` set | +1.5 us | +13 us |
| `TOG_OK` changes to the NAKed packet's result | +10 us | +57 us |

The NAKed packet is the host's next one, with the other PID, so `TOG_OK` turns 0.
Consequence: **`TOG_OK` describes the flag's own packet only while `IS_NAK` is 0.**

After the release:
- any completion rewrites `INT_ST`;
- an IN completion resets `RX_LEN` to 0, and an OUT on another endpoint sets it.

So only values read before the release are valid.

At full speed `TRANSFER` rises about 1.5 us before `SIE_FREE` returns, during the handshake.

### Response and toggle

- **`RES` is never changed by hardware.** There is no per-packet auto-NAK; flow control is
  entirely the driver's job.
- **`RES` = NYET (01)** ACKs-with-NYET and then accepts the next OUT within microseconds, into the
  same buffer. It is not a flow-control stop.
- **A toggle-mismatched OUT** (a retransmit) is ACKed and DMA-written over the buffer when `RES` =
  ACK, with `TRANSFER` raised, `TOG_OK` = 0 and `RX_LEN` set. With `RES` = NAK it is NAKed. (USB 2.0
  Table 8-6 expects an ACK in both cases.)
- **RX auto-toggle** (`R_TOG_AUTO`) flips `R_TOG` only on an accepted packet with the expected PID.
  The flip is visible by the time `TRANSFER` is.
- **TX auto-toggle** flips `T_TOG` when the host ACKs an IN.
- **EP0 has no auto-toggle**: writing its `TOG_AUTO` bits reads back 0 and `R_TOG` doesn't move.
  The manual marks them reserved for EP0.

### Bus reset

- A `TRANSFER` pending before a bus reset survives it, together with `BUS_RST`.
- Endpoint control registers are not reset by hardware.

## 2. Silicon: lost RX auto-toggle flip

`T_LEN` (16 bits), `TX_CTRL` and `RX_CTRL` of an endpoint share one 32-bit word. A CPU write to
`T_LEN` or `TX_CTRL` in the same HCLK cycle as the controller's RX auto-toggle flip on that
endpoint **drops the flip**:
- `R_TOG` keeps its old value;
- the CPU's byte is written correctly, so a read-back shows nothing wrong;
- the packet itself was ACKed and DMA-written normally, with `TOG_OK` = 1.

Measured with the CPU writing one register every 2 us while the host streams OUT to EP1 with
auto-toggle on:

| CPU write, every 2 us | OUT packets | lost flips |
| --- | --- | --- |
| none | 53 248 | 0 |
| `T_LEN1` (16-bit, same value / alternating) | 52 768 / 52 496 | 154 / 140 |
| `TX_CTRL1` (8-bit, same / alternating) | 52 432 / 52 816 | 23 / 29 |
| `MAX_LEN1` (another word) | 53 184 | 0 |
| `T_LEN2` (another endpoint) | 52 960 | 0 |

154 / 52 768 x 2 us is about 6 ns: one cycle at 144 MHz.

The converse test, alternating `RX_CTRL` toggle bits from the CPU while the host streams IN on
the same endpoint (and vice versa), showed no corruption of the CPU's byte in 2 000 000
read-backs. The controller does not write back a stale copy of the word; only its own flip is
lost. Whether RX-side writes drop *TX* flips was not measured. If they do, it self-heals: the IN
is resent with the same PID and the host drops the duplicate.

**How the driver hits it:** `endpoint.rs` `write()` writes `T_LEN` and `TX_CTRL` to arm the IN
side of an endpoint number while OUT packets complete on the same number. With no handling, the
handler sees "no flip" for an accepted packet, takes it for a retransmit and drops it. The
host's next packet then mismatches the stale `R_TOG` and is dropped too. That is two packets
lost, while the host saw both ACKed. A real failure, wire and driver trace side by side:

```
wire:   OUT DATA1[0B] ACK · IN DATA1[0B] ACK · OUT DATA0[16B] ACK · OUT DATA1[63B] ACK
driver: ZLP accepted, R_TOG flipped · IN done, write() re-arms IN · OUT 16 B: TOG_OK=1 but no flip
        → treated as retransmit → dropped; next DATA1 "mismatches" → dropped
```

It cannot be avoided by locking: the other writer is the controller. It can only be detected
afterwards (section 4), or avoided by not using auto-toggle on endpoints whose IN side is written
while OUT traffic runs. WCH's own examples toggle manually.

## 3. The interrupt handler, step by step

The `TRANSFER` branch of `InterruptHandler::on_interrupt`, for bulk / interrupt / control:

1. **`TRANSFER` together with `BUS_RST`**: release and drop it. It belongs to the session before
   the reset (it survives the reset, section 1).
2. **Read `SIE_FREE`, `INT_ST`, `RX_LEN`.** All latched except `tog_ok` / `is_nak` (section 1).
3. **NAK the completed direction** (`nak_non_iso`). This includes EP0, and happens whether or not
   the packet turns out to be new. Once released, the next packet lands within microseconds. If
   the endpoint is still at ACK it overwrites a buffer the task hasn't read. Skipping this on a
   "busy" entry was an EP0 bug: the next EP0 OUT landed in the shared 64-byte buffer.
4. **Release `TRANSFER` immediately.** Until then the whole bus is NAKed. An interrupt OUT the
   host polls right after an IN in the same microframe loses every poll in that window. About
   150 ns more before the release halved such a stream. Anything that can wait goes after the
   release.
5. **Decide acceptance** (section 4) from the values read in step 2 plus the endpoint's
   `R_TOG` / `T_TOG`, which can no longer move: the endpoint is NAKed.
6. **Deliver**: store the length, mark the endpoint ready, wake the task. The endpoint stays NAKed
   until `read()` / `write()` re-arms it. A non-accepted packet re-arms the endpoint at once if a
   `read()` / `write()` is still waiting (`rearm_if_waiting`).

What the handler must **not** do for non-iso entries:
- **Wait for `SIE_FREE` and re-read `INT_ST` / `RX_LEN` after the release.** An IN completion in
  between resets `RX_LEN`, and another endpoint's OUT sets it, so a re-read can return another
  transaction's length. An earlier version re-read on busy entries. The entry's values are final, because with `INT_BUSY` at most one
  non-iso transaction completes per flag.
- **Trust `TOG_OK` when `IS_NAK` is set.**

The iso branches (`busy`, `out_late`, `iso_out_check`, `on_sof`) handle transactions that ignore
`INT_BUSY` and may have completed before or after the entry. Their comments explain each case.

### Interrupt latency, measured

Delays of 2-114 us were injected at handler entry (high speed), with sequence-checked bulk,
interrupt and control traffic:
- Data stays intact; bulk only slows down.
- An interrupt OUT polled right after an IN in the same microframe gets nothing through once the
  handler is about 2 us late. That is inherent to `INT_BUSY`.
- The "ambiguous" counter (section 4) rose to 2-22 per run, all real lost flips. No duplicates
  were seen, but a retransmit in that window would be one.

At full speed, the same scenarios were run with simulated handler delays of 0 to 5 ms against the
register-level firmware, with the steps in the order above and, as a negative control, reversed
(release first, NAK later):

| Scenario | Order above | Reversed |
| --- | --- | --- |
| OUT stream | all packets, in order, at every delay | 3-11 missing and duplicated per 40 |
| OUT + IN on one endpoint number | all packets | losses and duplicates on both |
| IN stream | all packets | 2-5 stale packets resent as new |
| a second, late release 200 us after the first | — | up to 33 of 40 lost on *another* endpoint |
| 4-packet control write, late EP0 handler | all pass with `IS_NAK` checked | all fail from 50 us with `TOG_OK` alone |

## 4. OUT acceptance

`out_accepted()` for a bulk / interrupt OUT (EP1-15). `flipped` means `R_TOG` differs from the
value recorded at the last accepted packet (`EpState::tog`).

| `R_TOG` flipped | `IS_NAK` | `TOG_OK` | Meaning | Action |
| --- | --- | --- | --- | --- |
| yes | - | - | new packet | accept |
| no | 0 | 1 | new packet, flip lost (section 2) | flip `R_TOG` back in step, accept, count in `LOST_FLIPS` |
| no | 0 | 0 | retransmit (host missed our ACK) | drop, re-arm if a `read()` waits |
| no | 1 | any | `TOG_OK` belongs to a later NAKed packet | accept as new, correct `R_TOG`, count in `OUT_AMBIGUOUS` |

The last row is a judgement call. A lost flip needs only a CPU write in the wrong cycle, while a
retransmit needs a bus error, so "new" is far likelier. If it was a retransmit, the packet is
delivered twice. `OUT_AMBIGUOUS` stays at 0 when the handler is not delayed, so a nonzero value
in the field points at interrupt latency.

Correcting `R_TOG` is safe there: the endpoint is NAKed (step 3), so no RX flip can race the
write.

IN completions use `toggled()`: a flip means the host ACKed it. No flip means the packet is
resent with the same PID, and the host discards it if it already had it.

## 5. EP0

EP0 has no auto-toggle, so the driver flips `R_TOG` / `T_TOG` itself after each accepted stage.

- **Duplicate OUT:** `TOG_OK` = 0 *and* `IS_NAK` = 0. `TOG_OK` alone dropped good packets
  whenever the host's next, NAKed packet had rewritten it. Multi-packet control writes failed under
  bulk load that way.
- **`ep0_moved_on`:** the flag is one bit and `INT_ST` shows the latest token, NAKed ones too. A
  host polling a bulk IN right after an EP0 stage can therefore hide that stage's completion. The
  host's token for the other direction (the status stage) proves the data stage went through.
- **`SETUP_ACT`:** a new control transfer cancels whatever EP0 was doing. Both directions are NAKed
  and their state reset.

## 6. Rules for changing the code

- **Status reads:** read everything the decision needs before releasing `TRANSFER`, and nothing
  of `INT_ST` / `RX_LEN` after it.
- **NAK, then release:** NAK the completed direction before the release, and keep the code
  between the status read and the release minimal (step 4).
- **`TOG_OK`:** never use it without checking `IS_NAK`.
- **Writing an endpoint's word:** writing `T_LEN` / `TX_CTRL` of an endpoint can drop an RX flip
  on it at any time, and nothing makes that safe. Writing `R_TOG` from the CPU is safe only while
  the endpoint is NAKed.
- **Toggles:** don't rely on the controller to stop after one packet (no auto-NAK, NYET doesn't
  stop the next one).
- **Bus reset:** the hardware leaves endpoint registers alone; `bus_reset()` must set them.

## 7. Debugging

Symptoms and where to look:

| Symptom | Likely cause |
| --- | --- |
| Host sees every OUT succeed, data missing on the device, mostly with IN and OUT on one endpoint number | lost flip not handled (section 2) |
| Same, two consecutive packets, when the handler runs late | `TOG_OK` read with `IS_NAK` set |
| Wrong lengths under mixed IN/OUT traffic | `RX_LEN` read after the release |
| Corrupted or stale OUT data | endpoint left at ACK across the release (step 3) |
| Control writes fail under bulk load | EP0 duplicate test using `TOG_OK` alone |
| Interrupt OUT stream stalls under load | handler latency (section 3) |

`lost_toggle_flips()` gives (`LOST_FLIPS`, `OUT_AMBIGUOUS`). Both counters are useful in a test
fixture.

Testing methods that found the bugs above:
- sequence-checked streams in both directions on every endpoint type at once, with concurrent
  control transfers, for at least 60 s at high speed;
- Linux `usbtest` transfer cases;
- a trace ring in the driver, frozen on the first sequence error and dumped over a vendor
  request, plus a logic-analyzer capture triggered by a GPIO the firmware raises on the same
  error (full speed; the analyzer cannot decode high speed).

Before this fix, the high-speed mixed tests failed within about 1 000 to 1 500 frames. Short
or single-stream tests pass, so keep a mixed load running.

## 8. Known gaps

- **Not measured:**
  - TX-flip loss on RX-side writes;
  - the USBFS/OTG controller's auto-toggle;
  - host mode;
  - other chips.
- **Pre-existing bug, not fixed here:** a bulk IN `write()` pending when the host re-selects the
  current alternate setting never completes (`usbtest` case 2). Failing a pending write when its
  endpoint is re-enabled fixes it.
- **Not implemented:** full speed (the block is configured for high speed), remote wakeup,
  high-bandwidth isochronous, hardware double buffering.
