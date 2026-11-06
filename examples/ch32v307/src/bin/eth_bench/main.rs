//! Ethernet test app for the CH32V307 `eth::v1` driver: HTTP + WebSocket
//! streaming server, UDP test endpoint and register/PHY test endpoints.
//! Usage and host tools: `examples/ch32v307/eth_bench/README.md`.
//!
//! Chip-specific code (clocks, driver setup, register dumps, PHY tests) lives
//! in `chip.rs`.
//!
//! Port 80:
//! - `GET /`   -> embedded index.html (vanilla JS client)
//! - `GET /ws` -> WebSocket upgrade, then a stream of binary data frames
//! - `GET /stats` -> JSON device counters (see eth_bench/README.md)
//!
//! A producer task builds one frame every `period_us` and publishes it on a
//! pub/sub channel. Each WebSocket connection has a subscriber; if the client
//! (or the TCP link) can't keep up, the subscriber lags and frames are skipped.
//! Skipped frames show up as gaps in the sequence counter on the client side.
//!
//! Binary frame layout (little-endian):
//!   0  u8   magic  0x57 ('W')
//!   1  u8   kind   1 = data
//!   2  u16  n      number of u16 samples
//!   4  u32  seq    frame counter (global, monotonic)
//!   8  u32  t_us   device time when the frame was built (low 32 bits, µs)
//!  12  u32  lag    frames skipped for this connection so far (server side)
//!  16  u16[n]      ADC samples
//!
//! Client -> server (text): `cfg <period_us> <n_samples> [nagle]` changes the
//! global producer settings. The server answers every connection / cfg change
//! with a text message: `{"period_us":..,"n":..,"nagle":..,"max_n":..,..}`.

#![no_std]
#![no_main]

use core::fmt::Write as _;
use portable_atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_net::tcp::TcpSocket;
use embassy_net::{Stack, StackResources};
use core::cell::RefCell;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::pubsub::{PubSubChannel, WaitResult};
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::Write;
use embedded_websocket as ws;
use hal::{peripherals, println};
// `ConstStaticCell` builds the value in .bss. `StaticCell::init(big)` builds it
// on the stack first, and the main task frame grew to 25 KB, overlapping .bss.
use static_cell::ConstStaticCell;
pub(crate) use ch32_hal as hal;

mod chip;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Max samples per frame. 16 B header + 2*720 B = 1456 B payload, + 4 B WS
/// header = 1460 B = one TCP segment (MSS) on a 1500 B MTU.
const MAX_SAMPLES: usize = 720;
const HDR_LEN: usize = 16;
/// Connection handler tasks (HTTP or WS). Also = max WS subscribers.
const CONN_TASKS: usize = 3;
/// Frames buffered before a slow subscriber starts lagging.
const CHANNEL_DEPTH: usize = 4;
/// Frame ring: must be > CHANNEL_DEPTH so every queued seq is still stored.
const RING_DEPTH: usize = CHANNEL_DEPTH + 1;
const TCP_RX: usize = 512; // client only sends small control frames
const TCP_TX: usize = 4096;
const PAYLOAD_MAX: usize = HDR_LEN + 2 * MAX_SAMPLES;

static PERIOD_US: AtomicU32 = AtomicU32::new(10_000); // 100 frames/s
static N_SAMPLES: AtomicU32 = AtomicU32::new(64);
/// TCP Nagle algorithm on WS sockets (1 = on, smoltcp default). Off = one TCP
/// segment per frame: lower latency/jitter, more packets.
static NAGLE: AtomicU32 = AtomicU32::new(1);
/// Producer deadlines missed (frames that could not be produced on time).
static OVERRUNS: AtomicU32 = AtomicU32::new(0);

/// Static address (/24, no gateway) for a direct link to the test PC, or
/// `None` for DHCP.
const STATIC_IP: Option<[u8; 4]> = Some([192, 168, 1, 50]);

static INDEX_HTML: &[u8] = include_bytes!("index.html");

/// A frame already serialized in wire format (see module docs).
struct Frame {
    seq: u32,
    len: u16,
    data: [u8; PAYLOAD_MAX],
}

impl Frame {
    const fn new() -> Self {
        Self { seq: u32::MAX, len: 0, data: [0; PAYLOAD_MAX] }
    }
}

/// Frames live in a static ring; only the sequence number goes through the
/// pub/sub channel. Passing 1.4 KB frames by value put several copies on the
/// (8 KB) stack and overflowed it into .bss.
static RING: Mutex<CriticalSectionRawMutex, RefCell<[Frame; RING_DEPTH]>> =
    Mutex::new(RefCell::new([const { Frame::new() }; RING_DEPTH]));

type Chan = PubSubChannel<CriticalSectionRawMutex, u32, CHANNEL_DEPTH, CONN_TASKS, 1>;
static CHANNEL: Chan = PubSubChannel::new();
/// Frames a subscriber was told about but which were already overwritten.
static STALE: AtomicU32 = AtomicU32::new(0);
/// Client -> server bytes read and WS frames decoded (all sessions).
static WS_RX_BYTES: AtomicU32 = AtomicU32::new(0);
static WS_RX_FRAMES: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Interrupts / tasks
// ---------------------------------------------------------------------------

/// Periodic status line over SDI. SDI print blocks the CPU while the debugger
/// drains it (several ms per line), which shows up as stalls in the stream.
const STATUS_PRINT: bool = false; // use GET /stats instead
const STATUS_SECS: u64 = 5;
/// Per-connection event prints over SDI. Each character costs ~80 µs of blocked
/// CPU (the debugger drains SDI), i.e. 1-3 ms per line: enough to overflow the
/// RX descriptor ring during an ACK burst. Off by default.
const LOG_EVENTS: bool = false;
macro_rules! log {
    ($($t:tt)*) => { if LOG_EVENTS { println!($($t)*) } };
}

/// Spawn the CPU-headroom probe task (costs a little latency, see IDLE_LOOPS).
const IDLE_PROBE: bool = true;

type Device = Diag<chip::EthDevice>;

// ---------------------------------------------------------------------------
// Diagnostics: RX traffic mix, SMI cost, ring high-water, executor stalls
// ---------------------------------------------------------------------------

static OUR_MAC: Mutex<CriticalSectionRawMutex, core::cell::Cell<[u8; 6]>> =
    Mutex::new(core::cell::Cell::new([0; 6]));
static RX_BCAST: AtomicU32 = AtomicU32::new(0);
static RX_MCAST: AtomicU32 = AtomicU32::new(0);
static RX_UCAST_US: AtomicU32 = AtomicU32::new(0);
static RX_UCAST_OTHER: AtomicU32 = AtomicU32::new(0);
static TX_FRAMES: AtomicU32 = AtomicU32::new(0);
static SMI_CALLS: AtomicU32 = AtomicU32::new(0);
static SMI_US: AtomicU32 = AtomicU32::new(0);
/// Max CPU-owned RX descriptors seen in the ISR (reset on /diag read).
pub(crate) static RX_RING_HW: AtomicU32 = AtomicU32::new(0);
/// Max time between two idle-task iterations, µs (reset on /diag read).
static MAX_STALL_US: AtomicU32 = AtomicU32::new(0);
/// `receive()` returned None although RX frames were waiting (TX ring full).
static RX_BLOCKED_BY_TX: AtomicU32 = AtomicU32::new(0);
static RX_CALLS: AtomicU32 = AtomicU32::new(0);
/// Time of the oldest not-yet-consumed RX interrupt (µs, 0 = none), and the max
/// latency from RX interrupt to frame consumption (reset on /diag read).
pub(crate) static RX_PENDING_SINCE: AtomicU32 = AtomicU32::new(0);
static RX_MAX_LAT_US: AtomicU32 = AtomicU32::new(0);

/// Pass-through driver that counts frames and times `link_state()` (SMI read).
struct Diag<D>(D);
struct DiagRx<R>(R);
const RXLOG_N: usize = 32;
static mut RXLOG: [(u16, u32); RXLOG_N] = [(0, 0); RXLOG_N];
static mut RXLOG_IDX: u32 = 0;
struct DiagTx<T>(T);

impl<R: embassy_net_driver::RxToken> embassy_net_driver::RxToken for DiagRx<R> {
    fn consume<T, F: FnOnce(&mut [u8]) -> T>(self, f: F) -> T {
        let t = RX_PENDING_SINCE.swap(0, Ordering::Relaxed);
        if t != 0 {
            let lat = (Instant::now().as_micros() as u32).wrapping_sub(t);
            RX_MAX_LAT_US.fetch_max(lat, Ordering::Relaxed);
        }
        self.0.consume(|buf| {
            // log length + bytes 42..46 (UDP payload start) of every frame.
            let tag = if buf.len() >= 46 { u32::from_be_bytes([buf[42], buf[43], buf[44], buf[45]]) } else { 0 };
            critical_section::with(|_| unsafe {
                let i = RXLOG_IDX as usize % RXLOG_N;
                RXLOG[i] = (buf.len() as u16, tag);
                RXLOG_IDX = RXLOG_IDX.wrapping_add(1);
            });
            if buf.len() >= 6 {
                let d = &buf[..6];
                if d == [0xff; 6] {
                    RX_BCAST.fetch_add(1, Ordering::Relaxed);
                } else if d[0] & 1 != 0 {
                    RX_MCAST.fetch_add(1, Ordering::Relaxed);
                } else if OUR_MAC.lock(|m| m.get()) == d {
                    RX_UCAST_US.fetch_add(1, Ordering::Relaxed);
                } else {
                    RX_UCAST_OTHER.fetch_add(1, Ordering::Relaxed);
                }
            }
            f(buf)
        })
    }
}

impl<T: embassy_net_driver::TxToken> embassy_net_driver::TxToken for DiagTx<T> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        TX_FRAMES.fetch_add(1, Ordering::Relaxed);
        self.0.consume(len, f)
    }
}

impl<D: embassy_net_driver::Driver> embassy_net_driver::Driver for Diag<D> {
    type RxToken<'a> = DiagRx<D::RxToken<'a>> where Self: 'a;
    type TxToken<'a> = DiagTx<D::TxToken<'a>> where Self: 'a;
    fn receive(&mut self, cx: &mut core::task::Context) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        RX_CALLS.fetch_add(1, Ordering::Relaxed);
        let r = self.0.receive(cx);
        if r.is_none() && chip::rx_pending() {
            RX_BLOCKED_BY_TX.fetch_add(1, Ordering::Relaxed);
        }
        r.map(|(r, t)| (DiagRx(r), DiagTx(t)))
    }
    fn transmit(&mut self, cx: &mut core::task::Context) -> Option<Self::TxToken<'_>> {
        self.0.transmit(cx).map(DiagTx)
    }
    fn link_state(&mut self, cx: &mut core::task::Context) -> embassy_net_driver::LinkState {
        let t0 = Instant::now();
        let s = self.0.link_state(cx);
        SMI_US.fetch_add((Instant::now() - t0).as_micros() as u32, Ordering::Relaxed);
        SMI_CALLS.fetch_add(1, Ordering::Relaxed);
        s
    }
    fn capabilities(&self) -> embassy_net_driver::Capabilities {
        self.0.capabilities()
    }
    fn hardware_address(&self) -> embassy_net_driver::HardwareAddress {
        self.0.hardware_address()
    }
}

/// Shared buffer for JSON responses (/stats, /diag). A local buffer would live
/// across `.await` and be duplicated in every connection-task future.
static RESP_BUF: embassy_sync::mutex::Mutex<CriticalSectionRawMutex, FmtBuf<1024>> =
    embassy_sync::mutex::Mutex::new(FmtBuf { buf: [0; 1024], len: 0 });

/// Rough CPU headroom: loops of an always-ready task (platform-spin executor).
/// Compare against the idle baseline (no clients connected).
static IDLE_LOOPS: AtomicU32 = AtomicU32::new(0);

#[embassy_executor::task]
async fn idle_task() -> ! {
    let mut last = Instant::now();
    loop {
        IDLE_LOOPS.fetch_add(1, Ordering::Relaxed);
        check_irq_enable();
        embassy_futures::yield_now().await;
        let now = Instant::now();
        MAX_STALL_US.fetch_max((now - last).as_micros() as u32, Ordering::Relaxed);
        last = now;
    }
}

/// Times the global interrupt enable (mstatus.MIE) was found cleared in thread
/// mode outside any critical section, plus mcause/mepc of the last trap at that
/// moment (i.e. which interrupt returned without restoring MIE).
static MIE_LOST: AtomicU32 = AtomicU32::new(0);
static MIE_LOST_MCAUSE: AtomicU32 = AtomicU32::new(0);
static MIE_LOST_MEPC: AtomicU32 = AtomicU32::new(0);
/// Re-enable interrupts when lost. Needed with qingke 0.7 (its critical section
/// masked only MIE; with hardware nesting enabled, a nested interrupt could
/// leave MIE cleared). Fixed in qingke >= 0.8.3; kept as a counter only.
const MIE_REPAIR: bool = false;

fn check_irq_enable() {
    let mstatus: usize;
    unsafe { core::arch::asm!("csrr {}, mstatus", out(reg) mstatus) };
    if mstatus & 0x8 == 0 {
        let (mcause, mepc): (usize, usize);
        unsafe {
            core::arch::asm!("csrr {}, mcause", out(reg) mcause);
            core::arch::asm!("csrr {}, mepc", out(reg) mepc);
        }
        if MIE_LOST.fetch_add(1, Ordering::Relaxed) == 0 {
            MIE_LOST_MCAUSE.store(mcause as u32, Ordering::Relaxed);
            MIE_LOST_MEPC.store(mepc as u32, Ordering::Relaxed);
        }
        if MIE_REPAIR {
            unsafe { core::arch::asm!("csrsi mstatus, 0x8") };
        }
    }
}

// ---------------------------------------------------------------------------
// UDP test endpoint (port 5001), for isolating the V208 TX-completion issue:
//   "tx N LEN" -> N back-to-back datagrams of LEN bytes (u32 seq first), then
//                 "done <n_sent> <tx_timeouts>"
//   "E..."     -> echoed back unchanged (ping)
//   other      -> counted and dropped (flood)
// ---------------------------------------------------------------------------

static UDP_FLOOD: AtomicU32 = AtomicU32::new(0);
static UDP_ECHO: AtomicU32 = AtomicU32::new(0);
static UDP_TX: AtomicU32 = AtomicU32::new(0);

#[embassy_executor::task]
async fn udp_task(stack: Stack<'static>) -> ! {
    use embassy_net::udp::{PacketMetadata, UdpSocket};
    let mut rx_meta = [PacketMetadata::EMPTY; 8];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    // Sized for the V307 tests (small flood/echo frames); the V208's UDP TX
    // throughput test needs 1600 B here.
    let mut rx_buf = [0u8; 512];
    let mut tx_buf = [0u8; 512];
    let mut buf = [0u8; 512];
    let mut s = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    s.bind(5001).unwrap();
    loop {
        let Ok((n, meta)) = s.recv_from(&mut buf).await else { continue };
        let ep = meta.endpoint;
        if buf[0] == b'E' {
            UDP_ECHO.fetch_add(1, Ordering::Relaxed);
            let _ = s.send_to(&buf[..n], ep).await;
        } else if n > 3 && &buf[..3] == b"tx " {
            let mut it = core::str::from_utf8(&buf[3..n]).unwrap_or("").split_whitespace();
            let count: u32 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let len: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(64).clamp(8, buf.len());
            for i in 0..len {
                buf[i] = i as u8;
            }
            let mut sent = 0u32;
            for seq in 0..count {
                buf[..4].copy_from_slice(&seq.to_le_bytes());
                buf[4..8].copy_from_slice(b"SEQ!");
                if s.send_to(&buf[..len], ep).await.is_ok() {
                    sent += 1;
                    UDP_TX.fetch_add(1, Ordering::Relaxed);
                }
            }
            let mut m = FmtBuf::<64>::new();
            let _ = write!(m, "done {}", sent);
            let _ = s.send_to(m.as_bytes(), ep).await;
        } else {
            UDP_FLOOD.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Device>) -> ! {
    runner.run().await
}

const ADC_DMA_LEN: usize = 1024;
/// DMA1 channel 1 transfer counter (CH32V307 RM: DMA1 base 0x4002_0000 + 0x0C).
const DMA1_CH1_CNTR: *const u32 = 0x4002_000C as *const u32;

/// Builds one frame per period from the latest ADC samples and publishes it.
///
/// The ADC runs continuously (DMA circular buffer), so building a frame is
/// just a copy. The HAL has no ADC+DMA driver: it is wired up by hand here.
#[embassy_executor::task]
async fn producer_task(
    mut adc: hal::adc::Adc<'static, peripherals::ADC1>,
    mut pin: hal::Peri<'static, peripherals::PA1>,
    dma: hal::Peri<'static, peripherals::DMA1_CH1>,
) -> ! {
    // ADC clock: HAL leaves ADCPRE at reset (PCLK2/2 = 72 MHz, above the
    // 14 MHz max). /8 -> 9 MHz. 71.5 + 12.5 cycles -> ~107 kS/s.
    hal::pac::RCC.cfgr0().modify(|w| w.set_adcpre(hal::pac::rcc::vals::Adcpre::DIV8));
    adc.configure_channel(&mut pin, 1, hal::adc::SampleTime::CYCLES71_5, hal::adc::Pga::X1);
    static DMA_BUF: ConstStaticCell<[u16; ADC_DMA_LEN]> = ConstStaticCell::new([0; ADC_DMA_LEN]);
    let dma_buf = DMA_BUF.take();
    let dma_ptr = dma_buf.as_ptr();
    let mut ring = unsafe {
        hal::dma::ReadableRingBuffer::new(
            dma,
            (),
            hal::pac::ADC1.rdatar().as_ptr() as *mut u16,
            dma_buf,
            Default::default(),
        )
    };
    ring.start();
    hal::pac::ADC1.ctlr2().modify(|w| {
        w.set_dma(true);
        w.set_cont(true);
    });
    hal::pac::ADC1.ctlr2().modify(|w| w.set_swstart(true));


    let publisher = CHANNEL.immediate_publisher();
    static FRAME: ConstStaticCell<Frame> = ConstStaticCell::new(Frame::new());
    let frame = FRAME.take();
    let mut seq: u32 = 0;
    let mut next = Instant::now();

    loop {
        let period = Duration::from_micros(PERIOD_US.load(Ordering::Relaxed) as u64);
        next += period;
        let now = Instant::now();
        if next < now {
            // Can't keep up: don't try to catch up with a burst, just resync.
            // Must still yield, or this task starves the executor (net stack).
            OVERRUNS.fetch_add(1, Ordering::Relaxed);
            next = now;
            embassy_futures::yield_now().await;
        } else {
            Timer::at(next).await;
        }

        let n = (N_SAMPLES.load(Ordering::Relaxed) as usize).min(MAX_SAMPLES);
        let t_us = Instant::now().as_micros() as u32;
        let d = &mut frame.data;
        d[0] = 0x57;
        d[1] = 1;
        d[2..4].copy_from_slice(&(n as u16).to_le_bytes());
        d[4..8].copy_from_slice(&seq.to_le_bytes());
        d[8..12].copy_from_slice(&t_us.to_le_bytes());
        // d[12..16] (lag) is filled in per connection.
        // Latest n samples straight from the DMA buffer (no draining, no
        // history copy). Write position = LEN - CNTR (DMA1 channel 1 CNTR).
        let cntr = unsafe { core::ptr::read_volatile(DMA1_CH1_CNTR) } as usize & 0xFFFF;
        let wpos = (ADC_DMA_LEN - cntr) % ADC_DMA_LEN;
        let mut i = (wpos + ADC_DMA_LEN - n) % ADC_DMA_LEN;
        for c in d[HDR_LEN..HDR_LEN + 2 * n].chunks_exact_mut(2) {
            let v = unsafe { core::ptr::read_volatile(dma_ptr.add(i)) };
            c.copy_from_slice(&v.to_le_bytes());
            i = if i + 1 == ADC_DMA_LEN { 0 } else { i + 1 };
        }
        frame.len = (HDR_LEN + 2 * n) as u16;
        frame.seq = seq;
        // Short critical section: one memcpy of <= 1.5 KB.
        RING.lock(|r| {
            let slot = &mut r.borrow_mut()[seq as usize % RING_DEPTH];
            slot.seq = seq;
            slot.len = frame.len;
            slot.data[..frame.len as usize].copy_from_slice(&frame.data[..frame.len as usize]);
        });
        // Drops the oldest seq if a subscriber is behind -> it lags.
        publisher.publish_immediate(seq);
        seq = seq.wrapping_add(1);
    }
}

#[embassy_executor::task(pool_size = CONN_TASKS)]
async fn conn_task(id: usize, stack: Stack<'static>) -> ! {
    let mut rx = [0u8; TCP_RX];
    let mut tx = [0u8; TCP_TX];
    let mut buf = [0u8; 1024]; // HTTP request header / WS client frames
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(Duration::from_secs(10)));
        if socket.accept(80).await.is_err() {
            continue;
        }
        let remote = socket.remote_endpoint();
        match handle_conn(&mut socket, &mut buf).await {
            Ok(()) => {}
            Err(e) => log!("[{}] {:?}: {}", id, remote, e),
        }
        socket.close();
        let _ = socket.flush().await;
        socket.abort();
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

async fn handle_conn(socket: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<(), &'static str> {
    // Read the request header (must fit in `buf`).
    let mut len = 0;
    let hdr_len = loop {
        if len == buf.len() {
            write_status(socket, "431 Request Header Fields Too Large").await;
            return Err("header too large");
        }
        let n = socket.read(&mut buf[len..]).await.map_err(|_| "read")?;
        if n == 0 {
            return Err("eof in header");
        }
        len += n;
        if let Some(p) = buf[..len].windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };

    let mut headers = [httparse::EMPTY_HEADER; 24];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(&buf[..hdr_len]) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => {
            write_status(socket, "400 Bad Request").await;
            return Err("bad request");
        }
    }
    let path = req.path.unwrap_or("/");
    if req.method != Some("GET") {
        write_status(socket, "405 Method Not Allowed").await;
        return Ok(());
    }

    match path {
        "/" | "/index.html" => {
            let mut h = FmtBuf::<160>::new();
            let _ = write!(
                h,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                INDEX_HTML.len()
            );
            socket.write_all(h.as_bytes()).await.map_err(|_| "write")?;
            socket.write_all(INDEX_HTML).await.map_err(|_| "write")?;
            Ok(())
        }
        "/diag" => {
            let mut b = RESP_BUF.lock().await;
            b.len = 0;
            let _ = write!(
                b,
                "{{\"chip\":\"{}\",\"rx_bcast\":{},\"rx_mcast\":{},\"rx_ucast_us\":{},\"rx_ucast_other\":{},\"tx_frames\":{},\
\"smi_calls\":{},\"smi_us\":{},\"rx_ring_hw\":{},\"max_stall_us\":{},\"rx_calls\":{},\"rx_blocked_by_tx\":{},\"rx_max_lat_us\":{}",
                chip::CHIP,
                RX_BCAST.load(Ordering::Relaxed),
                RX_MCAST.load(Ordering::Relaxed),
                RX_UCAST_US.load(Ordering::Relaxed),
                RX_UCAST_OTHER.load(Ordering::Relaxed),
                TX_FRAMES.load(Ordering::Relaxed),
                SMI_CALLS.load(Ordering::Relaxed),
                SMI_US.load(Ordering::Relaxed),
                RX_RING_HW.swap(0, Ordering::Relaxed),
                MAX_STALL_US.swap(0, Ordering::Relaxed),
                RX_CALLS.load(Ordering::Relaxed),
                RX_BLOCKED_BY_TX.load(Ordering::Relaxed),
                RX_MAX_LAT_US.swap(0, Ordering::Relaxed),
            );
            chip::write_diag(&mut *b);
            let _ = b.write_str("}");
            let mut h = FmtBuf::<160>::new();
            let _ = write!(
                h,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                b.len
            );
            socket.write_all(h.as_bytes()).await.map_err(|_| "write")?;
            socket.write_all(b.as_bytes()).await.map_err(|_| "write")?;
            Ok(())
        }
        "/stats" => {
            let mut body = RESP_BUF.lock().await;
            body.len = 0;
            let _ = write!(
                body,
                "{{\"chip\":\"{}\",\"uptime_ms\":{},\"overruns\":{},\"stale\":{},\"stack_free\":{},\"intsyscr\":\"{:x}\",\"mie_lost\":{},\"mie_lost_mcause\":\"{:08x}\",\"mie_lost_mepc\":\"{:08x}\",\"idle_loops\":{},\"ws_rx_bytes\":{},\"ws_rx_frames\":{},\"udp_flood\":{},\"udp_echo\":{},\"udp_tx\":{},\"period_us\":{},\"n\":{},\"nagle\":{}",
                chip::CHIP,
                Instant::now().as_millis(),
                OVERRUNS.load(Ordering::Relaxed),
                STALE.load(Ordering::Relaxed),
                stack_free(),
                { let v: usize; unsafe { core::arch::asm!("csrr {}, 0x804", out(reg) v) }; v },
                MIE_LOST.load(Ordering::Relaxed),
                MIE_LOST_MCAUSE.load(Ordering::Relaxed),
                MIE_LOST_MEPC.load(Ordering::Relaxed),
                IDLE_LOOPS.load(Ordering::Relaxed),
                WS_RX_BYTES.load(Ordering::Relaxed),
                WS_RX_FRAMES.load(Ordering::Relaxed),
                UDP_FLOOD.load(Ordering::Relaxed),
                UDP_ECHO.load(Ordering::Relaxed),
                UDP_TX.load(Ordering::Relaxed),
                PERIOD_US.load(Ordering::Relaxed),
                N_SAMPLES.load(Ordering::Relaxed),
                NAGLE.load(Ordering::Relaxed),
            );
            chip::write_stats(&mut *body);
            let _ = body.write_str("}");
            let mut h = FmtBuf::<160>::new();
            let _ = write!(
                h,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len
            );
            socket.write_all(h.as_bytes()).await.map_err(|_| "write")?;
            socket.write_all(body.as_bytes()).await.map_err(|_| "write")?;
            Ok(())
        }
        "/ws" => {
            let ctx = ws::read_http_header(req.headers.iter().map(|h| (h.name, h.value)))
                .map_err(|_| "ws header")?;
            let Some(ctx) = ctx else {
                write_status(socket, "426 Upgrade Required").await;
                return Ok(());
            };
            // Leftover bytes after the header (client frames sent early).
            let leftover = len - hdr_len;
            buf.copy_within(hdr_len..len, 0);
            ws_session(socket, buf, leftover, &ctx).await
        }
        "/rxlog" => {
            // last RXLOG_N frames handed to the stack, oldest first: "len:tag" (tag hex).
            let mut b = RESP_BUF.lock().await;
            b.len = 0;
            critical_section::with(|_| unsafe {
                let n = RXLOG_IDX;
                for k in 0..RXLOG_N as u32 {
                    let (l, t) = RXLOG[(n.wrapping_add(k) as usize) % RXLOG_N];
                    let _ = write!(b, "{}:{:08x} ", l, t);
                }
            });
            let mut h = FmtBuf::<128>::new();
            let _ = write!(h, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len);
            socket.write_all(h.as_bytes()).await.map_err(|_| "write")?;
            socket.write_all(b.as_bytes()).await.map_err(|_| "write")?;
            Ok(())
        }
        _ if path.starts_with("/linktest/") => {
            // /linktest/<pn 0|1|2> (see chip::link_test); result via /linkresult.
            {
                let pn = path[10..].parse::<u16>().unwrap_or(2).min(2);
                write_status(socket, "202 Accepted").await;
                let _ = socket.flush().await;
                chip::link_test(pn).await;
            }
            Ok(())
        }
        "/linkresult" => {
            {
                let body = unsafe { &(&*core::ptr::addr_of!(chip::LINK_RESULT))[..chip::LINK_RESULT_LEN] };
                let mut h = FmtBuf::<160>::new();
                let _ = write!(h, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                socket.write_all(h.as_bytes()).await.map_err(|_| "write")?;
                socket.write_all(body).await.map_err(|_| "write")?;
            }
            Ok(())
        }
        _ if path.starts_with("/pntest/") => {
            // PHY polarity phases (see chip::pn_test), /pntest/<ms per phase>.
            {
                let ms = path[8..].parse::<u64>().unwrap_or(3000).clamp(100, 20_000);
                let mut b = RESP_BUF.lock().await;
                b.len = 0;
                let _ = b.write_str("{\"chip\":\"CH32V307\"");
                chip::pn_test(&mut *b, ms).await;
                let _ = b.write_str("}");
                let mut h = FmtBuf::<160>::new();
                let _ = write!(h, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len);
                socket.write_all(h.as_bytes()).await.map_err(|_| "write")?;
                socket.write_all(b.as_bytes()).await.map_err(|_| "write")?;
            }
            Ok(())
        }
        _ if path.starts_with("/peek/") || path.starts_with("/poke/") => {
            // TEST ONLY: raw 32-bit register access, hex: /peek/<addr>, /poke/<addr>/<value>.
            let mut it = path[6..].split('/');
            let addr = it.next().and_then(|v| u32::from_str_radix(v, 16).ok());
            let val = it.next().and_then(|v| u32::from_str_radix(v, 16).ok());
            let Some(addr) = addr.filter(|a| a % 4 == 0 && (0x4000_0000..0x6000_0000).contains(a)) else {
                write_status(socket, "400 Bad Request").await;
                return Ok(());
            };
            let p = addr as *mut u32;
            if path.starts_with("/poke/") {
                let Some(v) = val else {
                    write_status(socket, "400 Bad Request").await;
                    return Ok(());
                };
                unsafe { core::ptr::write_volatile(p, v) };
            }
            let v = unsafe { core::ptr::read_volatile(p) };
            let mut b = FmtBuf::<32>::new();
            let _ = write!(b, "{:08x}", v);
            let mut h = FmtBuf::<128>::new();
            let _ = write!(h, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len);
            socket.write_all(h.as_bytes()).await.map_err(|_| "write")?;
            socket.write_all(b.as_bytes()).await.map_err(|_| "write")?;
            Ok(())
        }
        _ => {
            write_status(socket, "404 Not Found").await;
            Ok(())
        }
    }
}

async fn write_status(socket: &mut TcpSocket<'_>, status: &str) {
    let mut h = FmtBuf::<128>::new();
    let _ = write!(h, "HTTP/1.1 {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", status);
    let _ = socket.write_all(h.as_bytes()).await;
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

async fn ws_session(
    socket: &mut TcpSocket<'_>,
    rxbuf: &mut [u8],
    mut rx_len: usize,
    ctx: &ws::WebSocketContext,
) -> Result<(), &'static str> {
    let mut server = ws::WebSocketServer::new_server();
    let mut out = [0u8; PAYLOAD_MAX + 16];
    let n = server
        .server_accept(&ctx.sec_websocket_key, None, &mut out)
        .map_err(|_| "ws accept")?;
    socket.write_all(&out[..n]).await.map_err(|_| "write")?;

    let mut sub = CHANNEL.subscriber().map_err(|_| "no subscriber slot")?;
    // Streaming connection: data keeps it alive, but a stalled client must not
    // hold the slot forever.
    socket.set_timeout(Some(Duration::from_secs(30)));
    log!("ws open");

    send_cfg(&mut server, socket, &mut out).await?;

    let mut lag: u32 = 0;
    let mut nagle = u32::MAX;
    let mut ctrl = [0u8; 128];

    loop {
        let want = NAGLE.load(Ordering::Relaxed);
        if want != nagle {
            socket.set_nagle_enabled(want != 0);
            nagle = want;
        }
        // Process any complete client frames already buffered.
        while rx_len > 0 {
            match server.read(&rxbuf[..rx_len], &mut ctrl) {
                Ok(r) => {
                    WS_RX_FRAMES.fetch_add(1, Ordering::Relaxed);
                    rxbuf.copy_within(r.len_from..rx_len, 0);
                    rx_len -= r.len_from;
                    match r.message_type {
                        ws::WebSocketReceiveMessageType::Text if r.end_of_message => {
                            if let Ok(s) = core::str::from_utf8(&ctrl[..r.len_to]) {
                                handle_cmd(s);
                            }
                            send_cfg(&mut server, socket, &mut out).await?;
                        }
                        ws::WebSocketReceiveMessageType::Ping => {
                            let n = server
                                .write(ws::WebSocketSendMessageType::Pong, true, &ctrl[..r.len_to], &mut out)
                                .map_err(|_| "ws encode")?;
                            socket.write_all(&out[..n]).await.map_err(|_| "write")?;
                        }
                        ws::WebSocketReceiveMessageType::CloseMustReply => {
                            let n = server
                                .write(ws::WebSocketSendMessageType::CloseReply, true, &ctrl[..r.len_to], &mut out)
                                .map_err(|_| "ws encode")?;
                            let _ = socket.write_all(&out[..n]).await;
                            log!("ws closed by client");
                            return Ok(());
                        }
                        ws::WebSocketReceiveMessageType::CloseCompleted => return Ok(()),
                        _ => {}
                    }
                }
                Err(ws::Error::ReadFrameIncomplete) => break,
                Err(e) => { log!("decode err {:?}", e); return Err("ws decode") }
            }
        }
        if rx_len == rxbuf.len() {
            return Err("client frame too large");
        }

        // Wait for either a new frame to send or incoming client data.
        // `select` is biased to its first future: put the socket read first,
        // otherwise a backlog of frames starves client frames (cfg/ping/close).
        let ev = {
            let read = socket.read(&mut rxbuf[rx_len..]);
            select(read, sub.next_message()).await
        };
        match ev {
            Either::Second(WaitResult::Lagged(n)) => lag = lag.wrapping_add(n as u32),
            Either::Second(WaitResult::Message(seq)) => {
                // Encode straight from the ring into `out`, then patch `lag`.
                let enc = RING.lock(|r| {
                    let r = r.borrow();
                    let f = &r[seq as usize % RING_DEPTH];
                    if f.seq != seq {
                        return None;
                    }
                    let len = f.len as usize;
                    let n = server.write(ws::WebSocketSendMessageType::Binary, true, &f.data[..len], &mut out);
                    Some(n.map(|n| (n, n - len)))
                });
                let Some(enc) = enc else {
                    STALE.fetch_add(1, Ordering::Relaxed);
                    lag = lag.wrapping_add(1);
                    continue;
                };
                let (n, hdr) = enc.map_err(|_| "ws encode")?;
                out[hdr + 12..hdr + 16].copy_from_slice(&lag.to_le_bytes());
                socket.write_all(&out[..n]).await.map_err(|_| "write")?;
            }
            Either::First(Ok(0)) => return Err("eof"),
            Either::First(Ok(n)) => {
                WS_RX_BYTES.fetch_add(n as u32, Ordering::Relaxed);
                rx_len += n
            }
            Either::First(Err(_)) => return Err("read"),
        }
    }
}

fn handle_cmd(s: &str) {
    let mut it = s.split_whitespace();
    if it.next() != Some("cfg") {
        return;
    }
    if let Some(p) = it.next().and_then(|v| v.parse::<u32>().ok()) {
        PERIOD_US.store(p.clamp(50, 10_000_000), Ordering::Relaxed);
    }
    if let Some(n) = it.next().and_then(|v| v.parse::<u32>().ok()) {
        N_SAMPLES.store(n.min(MAX_SAMPLES as u32), Ordering::Relaxed);
    }
    if let Some(v) = it.next().and_then(|v| v.parse::<u32>().ok()) {
        NAGLE.store((v != 0) as u32, Ordering::Relaxed);
    }
    log!(
        "cfg: period={}us n={} nagle={}",
        PERIOD_US.load(Ordering::Relaxed),
        N_SAMPLES.load(Ordering::Relaxed),
        NAGLE.load(Ordering::Relaxed)
    );
}

async fn send_cfg(
    server: &mut ws::WebSocketServer,
    socket: &mut TcpSocket<'_>,
    out: &mut [u8],
) -> Result<(), &'static str> {
    let mut msg = FmtBuf::<128>::new();
    let _ = write!(
        msg,
        "{{\"period_us\":{},\"n\":{},\"nagle\":{},\"max_n\":{},\"overruns\":{}}}",
        PERIOD_US.load(Ordering::Relaxed),
        N_SAMPLES.load(Ordering::Relaxed),
        NAGLE.load(Ordering::Relaxed),
        MAX_SAMPLES,
        OVERRUNS.load(Ordering::Relaxed)
    );
    let n = server
        .write(ws::WebSocketSendMessageType::Text, true, msg.as_bytes(), out)
        .map_err(|_| "ws encode")?;
    socket.write_all(&out[..n]).await.map_err(|_| "write")
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[embassy_executor::main(entry = "ch32_hal::entry")]
async fn main(spawner: Spawner) -> ! {
    stack_paint();
    hal::debug::SDIPrint::enable();
    let p = hal::init(chip::hal_config());

    Timer::after_millis(100).await;
    println!("ws-bench starting on {}", chip::CHIP);

    let (eth, mac_addr, seed) = chip::init(p.RNG, p.ETH);
    let device = Diag(eth);
    OUR_MAC.lock(|m| m.set(mac_addr));
    println!(
        "MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac_addr[0], mac_addr[1], mac_addr[2], mac_addr[3], mac_addr[4], mac_addr[5]
    );

    let net_config = match STATIC_IP {
        None => embassy_net::Config::dhcpv4(embassy_net::DhcpConfig::default()),
        Some(a) => embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
            address: embassy_net::Ipv4Cidr::new(embassy_net::Ipv4Address::new(a[0], a[1], a[2], a[3]), 24),
            gateway: None,
            dns_servers: Default::default(),
        }),
    };
    // Small, so a plain StaticCell is fine.
    static RESOURCES: static_cell::StaticCell<StackResources<{ CONN_TASKS + 2 }>> = static_cell::StaticCell::new();
    let (stack, runner) = embassy_net::new(device, net_config, RESOURCES.init(StackResources::new()), seed);
    spawner.spawn(net_task(runner).unwrap());

    let adc = hal::adc::Adc::new(p.ADC1, Default::default());
    spawner.spawn(producer_task(adc, p.PA1, p.DMA1_CH1).unwrap());

    println!("Waiting for link...");
    while !stack.is_link_up() {
        Timer::after_millis(100).await;
    }
    println!("Link up, waiting for DHCP...");
    stack.wait_config_up().await;
    if let Some(c) = stack.config_v4() {
        println!("IP: {}  ->  http://{}/", c.address, c.address.address());
    }

    chip::spawn_tasks(&spawner);
    spawner.spawn(udp_task(stack).unwrap());
    if IDLE_PROBE {
        spawner.spawn(idle_task().unwrap());
    }
    for id in 0..CONN_TASKS {
        spawner.spawn(conn_task(id, stack).unwrap());
    }

    let mut last_ip = None;
    loop {
        Timer::after_secs(STATUS_SECS).await;
        if !STATUS_PRINT {
            let ip = stack.config_v4().map(|c| c.address);
            if ip != last_ip {
                if let Some(a) = ip {
                    println!("IP: {}", a);
                }
                last_ip = ip;
            }
            continue;
        }
        let ip = stack.config_v4().map(|c| c.address);
        if ip != last_ip {
            if let Some(a) = ip {
                println!("IP: {}", a);
            }
            last_ip = ip;
        }
    }
}

// ---------------------------------------------------------------------------
// Stack high-water mark: paint the free stack area at boot, scan later.
// ---------------------------------------------------------------------------

const STACK_PAINT: u32 = 0xA5A5_A5A5;
extern "C" {
    static _ebss: u32;
}

fn stack_paint() {
    let sp: usize;
    unsafe { core::arch::asm!("mv {}, sp", out(reg) sp) };
    let mut p = unsafe { &_ebss as *const u32 as usize };
    while p < sp - 256 {
        unsafe { core::ptr::write_volatile(p as *mut u32, STACK_PAINT) };
        p += 4;
    }
}

/// Bytes of stack never touched since boot.
fn stack_free() -> usize {
    let start = unsafe { &_ebss as *const u32 as usize };
    let mut p = start;
    while unsafe { core::ptr::read_volatile(p as *const u32) } == STACK_PAINT {
        p += 4;
    }
    p - start
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("\nPANIC: {}", info);
    loop {}
}

// ---------------------------------------------------------------------------
// Small fixed-size formatting buffer
// ---------------------------------------------------------------------------

pub(crate) struct FmtBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> FmtBuf<N> {
    fn new() -> Self {
        Self { buf: [0; N], len: 0 }
    }
    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl<const N: usize> core::fmt::Write for FmtBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let b = s.as_bytes();
        if self.len + b.len() > N {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..self.len + b.len()].copy_from_slice(b);
        self.len += b.len();
        Ok(())
    }
}
