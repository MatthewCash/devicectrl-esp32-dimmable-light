#![no_std]
#![no_main]

extern crate alloc;

use core::{net::SocketAddrV4, str::FromStr};

use alloc::string::ToString;
use anyhow::Error;
use defmt::{error, println};
use defmt_rtt as _;
use devicectrl_common::protocol::simple::esp::{TransportChannels, transport_task};
use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources, StaticConfigV4};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    ecc::Ecc,
    gpio::{DriveMode, Level, Output, OutputConfig},
    interrupt::software::SoftwareInterruptControl,
    ledc::{
        self, LSGlobalClkSource, Ledc, LowSpeed,
        channel::{self, Channel, ChannelIFace},
        timer::{self, LSClockSource, TimerIFace, config::Duty},
    },
    rng::{Rng, Trng},
    sha::Sha,
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_radio::wifi::WifiDevice;
use esp32_ecdsa::CryptoContext;
use heapless::Vec;
use p256::{
    PublicKey, SecretKey,
    pkcs8::{DecodePrivateKey, DecodePublicKey},
};

use crate::{light::app_task, wifi::wifi_connection};

mod light;
mod wifi;

const DEVICE_ID: &str = env!("DEVICE_ID");

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

pub fn log_error(err: &Error) {
    error!("Error: {}", err.to_string().as_str());
    println!("Caused by:");

    err.chain().skip(1).enumerate().for_each(|(i, cause)| {
        println!("   {}: {}", i, cause.to_string().as_str());
    })
}

pub const SERVER_PUBLIC_KEY: &[u8] = include_bytes!(env!("SERVER_PUBLIC_KEY_PATH"));
pub const PRIVATE_KEY: &[u8] = include_bytes!(env!("PRIVATE_KEY_PATH"));

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::_80MHz));

    esp_alloc::heap_allocator!(size: 72 * 1024);

    let rng = Rng::new();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    // enable internal antenna
    Output::new(peripherals.GPIO3, Level::Low, OutputConfig::default());
    Timer::after(Duration::from_millis(100)).await;
    Output::new(peripherals.GPIO14, Level::Low, OutputConfig::default());

    let esp_radio_ctrl = &*mk_static!(
        esp_radio::Controller<'static>,
        esp_radio::init().expect("Failed to initialize radio controller")
    );

    let (controller, interfaces) =
        esp_radio::wifi::new(esp_radio_ctrl, peripherals.WIFI, Default::default())
            .expect("Failed to initialize wifi controller");

    let config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: env!("IP_CIDR").parse().unwrap(),
        gateway: None,
        dns_servers: Vec::new(),
    });

    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        interfaces.sta,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    let stack = mk_static!(Stack<'_>, stack);
    let runner = mk_static!(Runner<'_, WifiDevice<'_>>, runner);

    let crypto = CryptoContext {
        sha: Sha::new(peripherals.SHA),
        ecc: Ecc::new(peripherals.ECC),
        trng: Trng::try_new().expect("Failed to initialize TRNG"),
        secret_key: SecretKey::from_pkcs8_der(PRIVATE_KEY).expect("Failed to decode secret key"),
        server_public_key: PublicKey::from_public_key_der(SERVER_PUBLIC_KEY)
            .expect("Failed to decode server public key"),
    };

    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let lstimer0 = mk_static!(
        ledc::timer::Timer<'_, LowSpeed>,
        ledc.timer::<LowSpeed>(timer::Number::Timer0)
    );
    lstimer0
        .configure(timer::config::Config {
            duty: Duty::Duty7Bit, // ceil(log2(100))
            clock_source: LSClockSource::APBClk,
            frequency: Rate::from_khz(24),
        })
        .expect("Failed to configure LEDC timer");

    let led_channel = mk_static!(
        Channel<'_, LowSpeed>,
        ledc.channel(channel::Number::Channel0, peripherals.GPIO18)
    );

    led_channel
        .configure(channel::config::Config {
            timer: lstimer0,
            duty_pct: 100,
            drive_mode: DriveMode::PushPull,
        })
        .expect("Failed to configure LEDC channel");

    let transport = mk_static!(TransportChannels, TransportChannels::new());

    let device_id =
        devicectrl_common::DeviceId::from(DEVICE_ID).expect("Failed to create device id");

    let server_addr = SocketAddrV4::from_str(env!("SERVER_ADDR")).expect("Invalid server address");

    spawner.spawn(wifi_connection(controller)).unwrap();
    spawner.spawn(net_task(runner)).unwrap();
    spawner
        .spawn(transport_task(
            stack,
            server_addr,
            transport,
            device_id,
            crypto,
        ))
        .unwrap();
    spawner.spawn(app_task(led_channel, transport)).unwrap();
}

#[embassy_executor::task]
async fn net_task(runner: &'static mut Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}
