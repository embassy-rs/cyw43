#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![feature(async_fn_in_trait)]
#![allow(incomplete_features)]
#![feature(default_alloc_error_handler)]

use core::convert::Infallible;

use defmt::*;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Stack, StackResources};
use embassy_rp::adc::{Adc, Config};
use embassy_rp::interrupt;
use embassy_rp::gpio::{Flex, Level, Output};
use embassy_rp::peripherals::{PIN_23, PIN_24, PIN_25, PIN_29};
use embedded_hal_1::spi::ErrorType;
use embedded_hal_async::spi::{ExclusiveDevice, SpiBusFlush, SpiBusRead, SpiBusWrite};
use embedded_io::asynch::Write;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::{Channel, Receiver};

pub mod write_to {
    use core::cmp::min;
    use core::fmt;

    pub struct WriteTo<'a> {
        buffer: &'a mut [u8],
        // on write error (i.e. not enough space in buffer) this grows beyond
        // `buffer.len()`.
        used: usize,
    }

    impl<'a> WriteTo<'a> {
        pub fn new(buffer: &'a mut [u8]) -> Self {
            WriteTo { buffer, used: 0 }
        }

        pub fn as_str(self) -> Option<&'a str> {
            if self.used <= self.buffer.len() {
                // only successful concats of str - must be a valid str.
                use core::str::from_utf8_unchecked;
                Some(unsafe { from_utf8_unchecked(&self.buffer[..self.used]) })
            } else {
                None
            }
        }
    }

    impl<'a> fmt::Write for WriteTo<'a> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            if self.used > self.buffer.len() {
                return Err(fmt::Error);
            }
            let remaining_buf = &mut self.buffer[self.used..];
            let raw_s = s.as_bytes();
            let write_num = min(raw_s.len(), remaining_buf.len());
            remaining_buf[..write_num].copy_from_slice(&raw_s[..write_num]);
            self.used += raw_s.len();
            if write_num < raw_s.len() {
                Err(fmt::Error)
            } else {
                Ok(())
            }
        }
    }

    pub fn show<'a>(buffer: &'a mut [u8], args: fmt::Arguments) -> Result<&'a str, fmt::Error> {
        let mut w = WriteTo::new(buffer);
        fmt::write(&mut w, args)?;
        w.as_str().ok_or(fmt::Error)
    }
}

macro_rules! singleton {
    ($val:expr) => {{
        type T = impl Sized;
        static STATIC_CELL: StaticCell<T> = StaticCell::new();
        STATIC_CELL.init_with(move || $val)
    }};
}

#[embassy_executor::task]
async fn wifi_task(
    runner: cyw43::Runner<'static, Output<'static, PIN_23>, ExclusiveDevice<MySpi, Output<'static, PIN_25>>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    stack.run().await
}

static mut SOCKET: Option<TcpSocket> = None;
static mut DONE: bool = false;

static mut CHANNEL: Option<Channel<NoopRawMutex, [u8; 32], 1>> = None;
static mut RECEIVER: Option<Receiver<NoopRawMutex, [u8; 32], 1>> = None;

static mut RX_BUFFER: [u8; 4096] = [0; 4096];
static mut TX_BUFFER: [u8; 4096] = [0; 4096];

// TCP writer task
#[embassy_executor::task]
async fn send_buf() -> ! {

    loop {
        if unsafe {RECEIVER.is_none()} {
            // Should not get here
            error!("Spurious start of send_buf task");
            continue;
        }

        let rec = unsafe { &mut *RECEIVER.as_mut().unwrap() };
        // Wait here for bytes on the channel
        let buf = rec.recv().await;
        if unsafe { SOCKET.is_none() } {
            // Should not get here
            error!("Socket is not yet set for send_buf task");
            continue;
        }
        let socket = unsafe { &mut *SOCKET.as_mut().unwrap() };

        // Write the 32 bytes on the socket
        match socket.write_all(&buf).await {
            Ok(()) => {}
            Err(e) => {
                warn!("write error: {:?}", e);
                unsafe {
                    // Clear the socket static variable
                    SOCKET = None;
                    // Flag for downstream to know that the TCP connection failed
                    DONE = true;
                }
                continue;
            }
        };
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Hello World!");

    let p = embassy_rp::init(Default::default());

    // Include the WiFi firmware and Country Locale Matrix (CLM) blobs.
    let fw = include_bytes!("../../../firmware/43439A0.bin");
    let clm = include_bytes!("../../../firmware/43439A0_clm.bin");

    // To make flashing faster for development, you may want to flash the firmwares independently
    // at hardcoded addresses, instead of baking them into the program with `include_bytes!`:
    //     probe-rs-cli download 43439A0.bin --format bin --chip RP2040 --base-address 0x10100000
    //     probe-rs-cli download 43439A0.clm_blob --format bin --chip RP2040 --base-address 0x10140000
    //let fw = unsafe { core::slice::from_raw_parts(0x10100000 as *const u8, 224190) };
    //let clm = unsafe { core::slice::from_raw_parts(0x10140000 as *const u8, 4752) };

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let clk = Output::new(p.PIN_29, Level::Low);
    let mut dio = Flex::new(p.PIN_24);
    dio.set_low();
    dio.set_as_output();

    let irq = interrupt::take!(ADC_IRQ_FIFO);
    let mut adc = Adc::new(p.ADC, irq, Config::default());
    let mut p26 = p.PIN_26;

    let bus = MySpi { clk, dio };
    let spi = ExclusiveDevice::new(bus, cs);

    let state = singleton!(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;

    spawner.spawn(wifi_task(runner)).unwrap();

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

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
    let stack = &*singleton!(Stack::new(
        net_device,
        config,
        singleton!(StackResources::<1, 2, 8>::new()),
        seed
    ));

    unwrap!(spawner.spawn(net_task(stack)));
    // And now we can use it!

    // Create a new MPMC channel. Sends/receives 32x byte arrays, but only stores 1 of them at a time.
    let channel: Channel<NoopRawMutex, [u8; 32], 1> = Channel::new();

    // Flush the MPMC channel into static land.
    // The channel will live on as long as the program is running. Nothing is using the channel
    // yet so this is fine.
    unsafe {
        CHANNEL = Some(channel);
    }
    let sender = unsafe {
        // Split the now static channel and make the receiver static for use by the
        // Embassy task
        let channel = &mut *CHANNEL.as_mut().unwrap();
        let receiver = channel.receiver();
        let sender = channel.sender();

        // Put the channel receiver into the static value.
        RECEIVER = Some(receiver);
        sender
    };

    // Spawn off the the TCP writer task
    unwrap!(spawner.spawn(send_buf()));

    loop {
        // Mark that we're not done with this connection
        unsafe {
            DONE = false;
        }
        // Make a new socket. Can only get to this point if the current socket died
        let mut socket = unsafe {
            TcpSocket::new(stack, &mut RX_BUFFER, &mut TX_BUFFER)
        };
        socket.set_timeout(Some(embassy_net::SmolDuration::from_secs(100)));

        info!("Listening on TCP:1234...");
        if let Err(e) = socket.accept(1234).await {
            warn!("accept error: {:?}", e);
            continue;
        }

        info!("Received connection from {:?}", socket.remote_endpoint());

        // Put the socket into the static variable for use by the TCP writer task
        unsafe {
            SOCKET = Some(socket);
        }

        loop {
            // 32 bytes of buffer to be sent off to the TCP writer
            let mut buf = [0u8; 32];

            // RP2040 ADC is 12 bit, thus packed into a 16bit value.
            // Our buffer is 32 bytes => 16 ADC values
            for i in 0..16 {
                let val = adc.read(&mut p26).await;
                // Split off the MSB / LSB and put it into the buffer
                buf[i * 2] = (val & 0xFF) as u8;
                buf[i * 2 + 1] = ((val >> 8) & 0xFF) as u8;

                // Break the loop if we're done with this connection.
                if unsafe { DONE } {
                    break;
                }
            }

            // Break the loop if we're done with this connection
            if unsafe { DONE } {
                break;
            }

            // Send the 32 bytes to the TCP writer
            sender.send(buf).await;
        }
    }
}

struct MySpi {
    /// SPI clock
    clk: Output<'static, PIN_29>,

    /// 4 signals, all in one!!
    /// - SPI MISO
    /// - SPI MOSI
    /// - IRQ
    /// - strap to set to gSPI mode on boot.
    dio: Flex<'static, PIN_24>,
}

impl ErrorType for MySpi {
    type Error = Infallible;
}

impl SpiBusFlush for MySpi {
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl SpiBusRead<u32> for MySpi {
    async fn read(&mut self, words: &mut [u32]) -> Result<(), Self::Error> {
        self.dio.set_as_input();
        for word in words {
            let mut w = 0;
            for _ in 0..32 {
                w <<= 1;

                // rising edge, sample data
                if self.dio.is_high() {
                    w |= 0x01;
                }
                self.clk.set_high();

                // falling edge
                self.clk.set_low();
            }
            *word = w
        }

        Ok(())
    }
}

impl SpiBusWrite<u32> for MySpi {
    async fn write(&mut self, words: &[u32]) -> Result<(), Self::Error> {
        self.dio.set_as_output();
        for word in words {
            let mut word = *word;
            for _ in 0..32 {
                // falling edge, setup data
                self.clk.set_low();
                if word & 0x8000_0000 == 0 {
                    self.dio.set_low();
                } else {
                    self.dio.set_high();
                }

                // rising edge
                self.clk.set_high();

                word <<= 1;
            }
        }
        self.clk.set_low();

        self.dio.set_as_input();
        Ok(())
    }
}
