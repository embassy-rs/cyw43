#![no_std]
#![feature(type_alias_impl_trait)]

use cyw43_pio::PioSpi;
use embassy_rp::gpio::Output;
use embassy_rp::peripherals::{DMA_CH0, PIN_23, PIN_25};
use embassy_rp::pio::{Pio0, PioStateMachineInstance, Sm0};
use {defmt_rtt as _, panic_probe as _};

#[macro_export]
macro_rules! singleton {
    ($val:expr) => {{
        type T = impl Sized;
        static STATIC_CELL: StaticCell<T> = StaticCell::new();
        STATIC_CELL.init_with(move || $val)
    }};
}

#[embassy_executor::task]
pub async fn wifi_task(
    runner: cyw43::Runner<
        'static,
        Output<'static, PIN_23>,
        PioSpi<PIN_25, PioStateMachineInstance<Pio0, Sm0>, DMA_CH0>,
    >,
) -> ! {
    runner.run().await
}

/// Include the WiFi firmware and Country Locale Matrix (CLM) blobs.
pub fn include_firmware() -> (&'static [u8], &'static [u8]) {
    let fw = include_bytes!("../../../firmware/43439A0.bin");
    let clm = include_bytes!("../../../firmware/43439A0_clm.bin");

    // To make flashing faster for development, you may want to flash the firmwares independently
    // at hardcoded addresses, instead of baking them into the program with `include_bytes!`:
    //     probe-rs-cli download 43439A0.bin --format bin --chip RP2040 --base-address 0x10100000
    //     probe-rs-cli download 43439A0_clm.bin --format bin --chip RP2040 --base-address 0x10140000
    //let fw = unsafe { core::slice::from_raw_parts(0x10100000 as *const u8, 224190) };
    //let clm = unsafe { core::slice::from_raw_parts(0x10140000 as *const u8, 4752) };

    (fw, clm)
}
