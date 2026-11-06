//! CH32V307: Synopsys DWC EMAC + built-in 10M PHY (`ch32_hal::eth::v1`).
//!
//! Clocks, driver setup, `/stats` and `/diag` register dumps, and the PHY
//! polarity / link tests (`/pntest`, `/linktest`). See `eth_bench/README.md`.

use core::fmt::Write;

use embassy_executor::Spawner;
use hal::eth::{self, Ethernet, InternalPhy, PacketQueue};
use hal::rng::Rng;
use hal::{bind_interrupts, peripherals, rng};
use rand_core::RngCore;
use static_cell::ConstStaticCell;

use crate::hal;

pub const CHIP: &str = "CH32V307";

/// ETH DMA descriptors (TX, RX).
pub const ETH_TX_DESC: usize = 4;
pub const ETH_RX_DESC: usize = 4; // #231 example default; also makes RX overflow easy to trigger
pub type EthDevice = Ethernet<'static, peripherals::ETH, ETH_TX_DESC, ETH_RX_DESC, InternalPhy>;

bind_interrupts!(struct Irqs {
    RNG => rng::InterruptHandler<peripherals::RNG>;
    ETH => eth::InterruptHandler<peripherals::ETH>;
});

pub fn hal_config() -> hal::Config {
    let mut config = hal::Config::default();
    config.rcc = hal::rcc::Config::SYSCLK_FREQ_144MHZ_HSE;
    config
}

/// Creates the Ethernet device. Returns (device, MAC, TCP seed).
pub fn init(
    rng: hal::Peri<'static, peripherals::RNG>,
    eth: hal::Peri<'static, peripherals::ETH>,
) -> (EthDevice, [u8; 6], u64) {
    let mut rng = Rng::new(rng, Irqs);
    let seed = rng.next_u64();

    // Locally administered MAC derived from the chip unique ID.
    let uid = hal::signature::unique_id();
    let mac = [0x02, uid[0] ^ uid[6], uid[1] ^ uid[7], uid[2] ^ uid[8], uid[3] ^ uid[9], uid[4] ^ uid[10]];

    // `ConstStaticCell` builds the value in .bss (a `StaticCell::init(big)`
    // builds it on the stack first and overflowed the main task stack).
    static QUEUE: ConstStaticCell<PacketQueue<ETH_TX_DESC, ETH_RX_DESC>> = ConstStaticCell::new(PacketQueue::new());
    let device = Ethernet::new_internal(eth, Irqs, QUEUE.take(), mac).expect("eth init");
    (device, mac, seed)
}

/// Tasks to start once the network is up.
pub fn spawn_tasks(spawner: &Spawner) {
    let _ = spawner;
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// DMA current host RX descriptor pointer.
const ETH_DMACHRDR: *const u32 = 0x4002_904C as *const u32;

/// Count CPU-owned descriptors in the RX ring (walk from DMA's current one).
unsafe fn rx_ring_occupancy() -> u32 {
    let mut d = core::ptr::read_volatile(ETH_DMACHRDR) as *const u32;
    let mut n = 0;
    for _ in 0..ETH_RX_DESC {
        if core::ptr::read_volatile(d) & (1 << 31) == 0 {
            n += 1;
        }
        d = core::ptr::read_volatile(d.add(3)) as *const u32;
    }
    n
}

/// Frames waiting in the hardware RX ring (for the "blocked by TX" counter).
pub fn rx_pending() -> bool {
    unsafe { rx_ring_occupancy() > 0 }
}

fn reg(addr: u32) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// Raw SMI read (PHY address 1 = internal PHY). Keeps the CR divider the HAL set.
fn smi_read(reg: u32) -> u16 {
    const MIIAR: *mut u32 = 0x4002_8010 as *mut u32;
    const MIIDR: *const u32 = 0x4002_8014 as *const u32;
    critical_section::with(|_| unsafe {
        let cr = core::ptr::read_volatile(MIIAR) & (0x7 << 2);
        core::ptr::write_volatile(MIIAR, (1 << 11) | (reg << 6) | cr | 1);
        while core::ptr::read_volatile(MIIAR) & 1 != 0 {}
        core::ptr::read_volatile(MIIDR) as u16
    })
}

/// Chip-specific fields for /stats (each prefixed with a comma).
pub fn write_stats(w: &mut impl Write) {
    let _ = write!(
        w,
        ",\"mmc_rx_good_ucast\":{},\"mmc_rx_crc_err\":{},\"mmc_rx_align_err\":{},\"dmasr\":\"{:08x}\"",
        reg(0x4002_81C4),
        reg(0x4002_8194),
        reg(0x4002_8198),
        reg(0x4002_9014),
    );
}

/// Chip-specific fields for /diag (each prefixed with a comma).
pub fn write_diag(w: &mut impl Write) {
    let _ = write!(
        w,
        ",\"maccr\":\"{:08x}\",\"macffr\":\"{:08x}\",\"dmabmr\":\"{:08x}\",\"dmaomr\":\"{:08x}\",\"dmasr\":\"{:08x}\",\
\"mmc_rx_crc_err\":{},\"mmc_rx_align_err\":{},\"mmc_rx_good_ucast\":{},\"mmc_tx_good\":{},\
\"phy_bmcr\":\"{:04x}\",\"phy_bmsr\":\"{:04x}\",\"phy_anar\":\"{:04x}\",\"phy_anlpar\":\"{:04x}\",\"phy_status\":\"{:04x}\",\"phy_mdix\":\"{:04x}\"",
        reg(0x4002_8000),
        reg(0x4002_8004),
        reg(0x4002_9000),
        reg(0x4002_9018),
        reg(0x4002_9014),
        reg(0x4002_8194),
        reg(0x4002_8198),
        reg(0x4002_81C4),
        reg(0x4002_8168),
        smi_read(0),
        smi_read(1),
        smi_read(4),
        smi_read(5),
        smi_read(0x10),
        smi_read(0x1E),
    );
}

// ---------------------------------------------------------------------------
// PHY polarity tests (internal 10M PHY, MDIX register 0x1E bits 3:2)
// ---------------------------------------------------------------------------

fn wr(addr: u32, v: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
}

fn smi_write(r: u32, val: u16) {
    const MIIAR: u32 = 0x4002_8010;
    const MIIDR: u32 = 0x4002_8014;
    critical_section::with(|_| {
        let cr = reg(MIIAR) & (0x7 << 2);
        wr(MIIDR, val as u32);
        wr(MIIAR, (1 << 11) | (r << 6) | cr | 0b11);
        while reg(MIIAR) & 1 != 0 {}
    })
}

/// PHY receive polarity. Phases of `ms` each: forced normal (MDIX bits
/// 3:2 = 00), forced reversed (01), automatic (10, starting from the reversed
/// state). Per phase: MAC good-unicast and CRC-error counter deltas, PHY link
/// bit at the end, MDIX value. Restores the original MDIX value at the end.
pub async fn pn_test(w: &mut impl Write, ms: u64) {
    const MDIX: u32 = 0x1E;
    const MMC_UCAST: u32 = 0x4002_81C4;
    const MMC_CRC: u32 = 0x4002_8194;
    let orig = smi_read(MDIX);
    let _ = write!(w, ",\"mdix_orig\":\"{:04x}\",\"phases\":[", orig);
    for (i, (name, pn)) in [("normal", 0u16), ("reversed", 1), ("auto", 2)].into_iter().enumerate() {
        smi_write(MDIX, (orig & !(3 << 2)) | (pn << 2));
        let (u0, c0) = (reg(MMC_UCAST), reg(MMC_CRC));
        embassy_time::Timer::after_millis(ms).await;
        let (u1, c1) = (reg(MMC_UCAST), reg(MMC_CRC));
        let _ = write!(
            w,
            "{}{{\"phase\":\"{}\",\"mdix\":\"{:04x}\",\"good_ucast\":{},\"crc_err\":{},\"link\":{},\"status\":\"{:04x}\"}}",
            if i == 0 { "" } else { "," },
            name,
            smi_read(MDIX),
            u1.wrapping_sub(u0),
            c1.wrapping_sub(c0),
            smi_read(1) & (1 << 2) != 0,
            smi_read(0x10),
        );
    }
    smi_write(MDIX, orig);
    let _ = w.write_str("]");
}

/// Result of the last `link_test` (read back with /linkresult).
pub static mut LINK_RESULT: [u8; 384] = [0; 384];
pub static mut LINK_RESULT_LEN: usize = 0;

/// Cable "replug" with a given receive polarity. Sets MDIX bits 3:2
/// (0 normal, 1 reversed, 2 auto), powers the PHY down for 1 s, powers it up
/// with auto-negotiation, then measures time to link-up and, for 2 s after it,
/// the MAC's good-unicast / CRC-error counters. Restores MDIX auto at the end.
pub async fn link_test(pn: u16) {
    use embassy_time::{Instant, Timer};
    const MDIX: u32 = 0x1E;
    let orig = smi_read(MDIX);
    smi_write(MDIX, (orig & !(3 << 2)) | (pn << 2));
    smi_write(0, 1 << 11); // power down: link partner sees the cable unplugged
    Timer::after_millis(1000).await;
    let mdix_after_pd = smi_read(MDIX);
    smi_write(0, (1 << 12) | (1 << 9)); // power up, autoneg + restart
    let t0 = Instant::now();
    let mut t_link = None;
    let mut last_print = 0u64;
    while t0.elapsed().as_millis() < 8000 {
        let _ = smi_read(1); // BSR link bit is latched low: read twice
        let el = t0.elapsed().as_millis();
        if el >= last_print {
            last_print = el + 500;
            hal::println!("lt {} ms: bmcr {:04x} bmsr {:04x} anlpar {:04x} st {:04x} mdix {:04x}", el, smi_read(0), smi_read(1), smi_read(5), smi_read(0x10), smi_read(MDIX));
        }
        if smi_read(1) & (1 << 2) != 0 {
            t_link = Some(t0.elapsed().as_millis());
            break;
        }
        Timer::after_millis(10).await;
    }
    let (u0, c0) = (reg(0x4002_81C4), reg(0x4002_8194));
    Timer::after_millis(2000).await;
    let (u1, c1) = (reg(0x4002_81C4), reg(0x4002_8194));
    let (mdix, status, anlpar, bmsr) = (smi_read(MDIX), smi_read(0x10), smi_read(5), smi_read(1));
    hal::println!("lt result: link_ms {:?} good {} crc {} mdix {:04x} st {:04x} anlpar {:04x} bmsr {:04x}", t_link, u1.wrapping_sub(u0), c1.wrapping_sub(c0), mdix, status, anlpar, bmsr);
    // Restore automatic polarity; if the link is not up, replug once more.
    smi_write(MDIX, (orig & !(3 << 2)) | (2 << 2));
    if bmsr & (1 << 2) == 0 {
        smi_write(0, 1 << 11);
        Timer::after_millis(500).await;
        smi_write(0, (1 << 12) | (1 << 9));
        let t1 = Instant::now();
        while t1.elapsed().as_millis() < 6000 {
            let _ = smi_read(1);
            hal::println!("lt restore {} ms: bmcr {:04x} bmsr {:04x} anlpar {:04x} mdix {:04x}", t1.elapsed().as_millis(), smi_read(0), smi_read(1), smi_read(5), smi_read(MDIX));
            if smi_read(1) & (1 << 2) != 0 {
                break;
            }
            Timer::after_millis(500).await;
        }
    }
    let mut b = crate::FmtBuf::<384>::new();
    let _ = write!(
        b,
        "{{\"pn\":{},\"mdix_set\":\"{:04x}\",\"mdix_after_powerdown\":\"{:04x}\",\"link_ms\":{},\"good_ucast_2s\":{},\"crc_err_2s\":{},\"mdix\":\"{:04x}\",\"status\":\"{:04x}\",\"anlpar\":\"{:04x}\",\"bmsr\":\"{:04x}\"}}",
        pn,
        (orig & !(3 << 2)) | (pn << 2),
        mdix_after_pd,
        t_link.map(|t| t as i64).unwrap_or(-1),
        u1.wrapping_sub(u0),
        c1.wrapping_sub(c0),
        mdix,
        status,
        anlpar,
        bmsr
    );
    unsafe {
        let n = b.as_bytes().len().min(384);
        (&mut *core::ptr::addr_of_mut!(LINK_RESULT))[..n].copy_from_slice(&b.as_bytes()[..n]);
        LINK_RESULT_LEN = n;
    }
}
