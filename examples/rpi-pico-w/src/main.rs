#![no_std]
#![no_main]
#![feature(generic_associated_types, type_alias_impl_trait)]

use core::cell::RefCell;
use core::convert::Infallible;
use core::future::Future;

use defmt::{assert, assert_eq, panic, *};
use embassy::executor::Spawner;
use embassy::time::{Duration, Timer};
use embassy::util::Forever;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Ipv4Address, Ipv4Cidr, Stack, StackResources};
use embassy_rp::gpio::{Flex, Level, Output, Pin};
use embassy_rp::peripherals::{PIN_23, PIN_24, PIN_25, PIN_29};
use embassy_rp::Peripherals;
use embedded_hal_1::digital::blocking::InputPin;
use embedded_hal_1::digital::ErrorType;
use embedded_hal_1::spi::ErrorType as SpiErrorType;
use embedded_hal_async::spi::{ExclusiveDevice, SpiBusFlush, SpiBusRead, SpiBusWrite};
use embedded_io::asynch::{Read, Write};
use {defmt_rtt as _, panic_probe as _};

macro_rules! forever {
    ($val:expr) => {{
        type T = impl Sized;
        static FOREVER: Forever<T> = Forever::new();
        FOREVER.put_with(move || $val)
    }};
}

#[embassy::task]
async fn wifi_task(
    runner: cyw43::Runner<
        'static,
        Output<'static, PIN_23>,
        MyIrq<'static>,
        ExclusiveDevice<MySpi<'static>, Output<'static, PIN_25>>,
    >,
) -> ! {
    runner.run().await
}

#[embassy::task]
async fn net_task(stack: &'static Stack<cyw43::NetDevice<'static>>) -> ! {
    stack.run().await
}

#[embassy::main]
async fn main(spawner: Spawner, p: Peripherals) {
    info!("Hello World!");

    // Include the WiFi firmware and CLM.
    //let fw = include_bytes!("../../../firmware/43439A0.bin");
    //let clm = include_bytes!("../../../firmware/43439A0_clm.bin");

    // To make flashing faster for development, you may want to flash the firmwares independently
    // at hardcoded addresses, instead of baking them into the program with `include_bytes!`:
    //     probe-rs-cli download 43439A0.bin --format bin --chip RP2040 --base-address 0x10100000
    //     probe-rs-cli download 43439A0.clm_blob --format bin --chip RP2040 --base-address 0x10140000
    let fw = unsafe { core::slice::from_raw_parts(0x10100000 as *const u8, 224190) };
    let clm = unsafe { core::slice::from_raw_parts(0x10140000 as *const u8, 4752) };

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let clk = Output::new(p.PIN_29, Level::Low);
    let mut dio = Flex::new(p.PIN_24);
    dio.set_low();
    dio.set_as_output();

    let inner = forever!(RefCell::new(Inner { clk, dio }));

    let bus = MySpi { inner };
    let spi = ExclusiveDevice::new(bus, cs);

    let irq = MyIrq { inner };

    let state = forever!(cyw43::State::new());
    let (mut control, runner) = cyw43::new(state, pwr, irq, spi, fw).await;

    spawner.spawn(wifi_task(runner)).unwrap();

    let net_device = control.init(clm).await;

    //control.join_open(env!("WIFI_NETWORK")).await;
    control.join_wpa2(env!("WIFI_NETWORK"), env!("WIFI_PASSWORD")).await;

    let config = embassy_net::ConfigStrategy::Dhcp;
    //let config = embassy_net::ConfigStrategy::Static(embassy_net::Config {
    //    address: Ipv4Cidr::new(Ipv4Address::new(192, 168, 69, 2), 24),
    //    dns_servers: Vec::new(),
    //    gateway: Some(Ipv4Address::new(192, 168, 69, 1)),
    //});

    // Generate random seed
    let seed = 0x0123_4567_89ab_cdef; // chosen by fair dice roll. guarenteed to be random.

    // Init network stack
    let stack = &*forever!(Stack::new(
        net_device,
        config,
        forever!(StackResources::<1, 2, 8>::new()),
        seed
    ));

    unwrap!(spawner.spawn(net_task(stack)));

    // And now we can use it!

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    let mut buf = [0; 4096];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(embassy_net::SmolDuration::from_secs(10)));

        info!("Listening on TCP:1234...");
        if let Err(e) = socket.accept(1234).await {
            warn!("accept error: {:?}", e);
            continue;
        }

        info!("Received connection from {:?}", socket.remote_endpoint());

        loop {
            let n = match socket.read(&mut buf).await {
                Ok(0) => {
                    warn!("read EOF");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    warn!("read error: {:?}", e);
                    break;
                }
            };

            info!("rxd {:02x}", &buf[..n]);

            match socket.write_all(&buf[..n]).await {
                Ok(()) => {}
                Err(e) => {
                    warn!("write error: {:?}", e);
                    break;
                }
            };
        }
    }
}

struct Inner {
    /// SPI clock
    clk: Output<'static, PIN_29>,

    /// 4 signals, all in one!!
    /// - SPI MISO
    /// - SPI MOSI
    /// - IRQ
    /// - strap to set to gSPI mode on boot.
    dio: Flex<'static, PIN_24>,
}

struct MyIrq<'d> {
    inner: &'d RefCell<Inner>,
}

impl<'d> ErrorType for MyIrq<'d> {
    type Error = Infallible;
}

impl<'d> InputPin for MyIrq<'d> {
    fn is_high(&self) -> Result<bool, Self::Error> {
        Ok(self.inner.borrow_mut().dio.is_high())
    }

    fn is_low(&self) -> Result<bool, Self::Error> {
        Ok(!self.is_high()?)
    }
}

struct MySpi<'d> {
    inner: &'d RefCell<Inner>,
}

impl<'d> SpiErrorType for MySpi<'d> {
    type Error = Infallible;
}

impl<'d> SpiBusFlush for MySpi<'d> {
    type FlushFuture<'a> = impl Future<Output = Result<(), Self::Error>>
    where
        Self: 'a;

    fn flush<'a>(&'a mut self) -> Self::FlushFuture<'a> {
        async move { Ok(()) }
    }
}

impl<'d> SpiBusRead<u32> for MySpi<'d> {
    type ReadFuture<'a> = impl Future<Output = Result<(), Self::Error>>
    where
        Self: 'a;

    fn read<'a>(&'a mut self, words: &'a mut [u32]) -> Self::ReadFuture<'a> {
        async move {
            let s = &mut *self.inner.borrow_mut();
            s.dio.set_as_input();
            for word in words {
                let mut w = 0;
                for _ in 0..32 {
                    w = w << 1;

                    // rising edge, sample data
                    if s.dio.is_high() {
                        w |= 0x01;
                    }
                    s.clk.set_high();

                    // falling edge
                    s.clk.set_low();
                }
                *word = w
            }

            Ok(())
        }
    }
}

impl<'d> SpiBusWrite<u32> for MySpi<'d> {
    type WriteFuture<'a> = impl Future<Output = Result<(), Self::Error>>
    where
        Self: 'a;

    fn write<'a>(&'a mut self, words: &'a [u32]) -> Self::WriteFuture<'a> {
        async move {
            let s = &mut *self.inner.borrow_mut();
            s.dio.set_as_output();
            for word in words {
                let mut word = *word;
                for _ in 0..32 {
                    // falling edge, setup data
                    s.clk.set_low();
                    if word & 0x8000_0000 == 0 {
                        s.dio.set_low();
                    } else {
                        s.dio.set_high();
                    }

                    // rising edge
                    s.clk.set_high();

                    word = word << 1;
                }
            }
            s.clk.set_low();

            s.dio.set_as_input();
            Ok(())
        }
    }
}
