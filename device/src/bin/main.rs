#![no_std]
#![no_main]
#![deny(clippy::mem_forget, reason = "not safe with esp_hal types")]
#![deny(clippy::large_stack_frames)]

use bt_hci::controller::ExternalController;
use defmt::{info, unwrap};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::Level;
use esp_hal::rmt::{PulseCode, TxChannelConfig, TxChannelCreator};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::controller::BleConnector;
use trouble_host::prelude::*;
use trouble_host::IoCapabilities;

extern crate alloc;

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2;
const MAX_ATTRIBUTES: usize = 32;
const BYTES_PER_PIXEL: usize = 3;
const MAX_PIXELS: usize = 256;
const MAX_PULSES: usize = MAX_PIXELS * BYTES_PER_PIXEL * 8;

const WS2812_T0H: u16 = 7;
const WS2812_T0L: u16 = 16;
const WS2812_T1H: u16 = 14;
const WS2812_T1L: u16 = 12;

const LED_SERVICE_UUID: Uuid = Uuid::new_long([
    0xaa, 0xbb, 0xcc, 0xde, 0x12, 0x34, 0x56, 0x78, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0,
]);

const LED_DATA_UUID: Uuid = Uuid::new_long([
    0xaa, 0xbb, 0xcc, 0xde, 0x12, 0x34, 0x56, 0x78, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0,
]);

const LED_SHOW_UUID: Uuid = Uuid::new_long([
    0xaa, 0xbb, 0xcc, 0xdf, 0x12, 0x34, 0x56, 0x78, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0,
]);

type BleStack =
    trouble_host::Stack<'static, ExternalController<BleConnector<'static>, 1>, DefaultPacketPool>;

type GattServer =
    AttributeServer<'static, NoopRawMutex, DefaultPacketPool, MAX_ATTRIBUTES, 0, CONNECTIONS_MAX>;

type RmtChannel = esp_hal::rmt::Channel<'static, esp_hal::Blocking, esp_hal::rmt::Tx>;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

esp_bootloader_esp_idf::esp_app_desc!();

static mut RMT_CHANNEL: Option<RmtChannel> = None;

fn ws2812_pulses(pixels: &[u8], pulses: &mut [PulseCode; MAX_PULSES]) -> usize {
    let mut n = 0;
    for &byte in pixels {
        for bit in (0..8).rev() {
            let (hi, lo) = if (byte >> bit) & 1 == 1 {
                (WS2812_T1H, WS2812_T1L)
            } else {
                (WS2812_T0H, WS2812_T0L)
            };
            if n < MAX_PULSES {
                pulses[n] = PulseCode::new(Level::High, hi, Level::Low, lo);
                n += 1;
            }
        }
    }
    n
}

fn flush_leds(pixel_buf: &[u8], pulse_buf: &mut [PulseCode; MAX_PULSES]) {
    unsafe {
        let rmt = &mut *core::ptr::addr_of_mut!(RMT_CHANNEL);
        if let Some(ch) = rmt.take() {
            let n = ws2812_pulses(pixel_buf, pulse_buf);
            match ch.transmit(&pulse_buf[..n]) {
                Ok(tx) => match tx.wait() {
                    Ok(new_ch) => *rmt = Some(new_ch),
                    Err((_e, new_ch)) => *rmt = Some(new_ch),
                },
                Err(_e) => info!("RMT transmit failed"),
            }
        }
    }
}

#[embassy_executor::task]
async fn ble_runner_task(stack: &'static BleStack) {
    let host = stack.build();
    let mut runner = host.runner;
    let _ = runner.run().await;
}

#[embassy_executor::task]
async fn gatt_server_task(
    stack: &'static BleStack,
    server: &'static GattServer,
    led_data_char: Characteristic<[u8; 1]>,
    led_show_char: Characteristic<[u8; 1]>,
) {
    Timer::after(Duration::from_millis(100)).await;

    let mut host = stack.build();
    let mut pixel_buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let mut pulse_buf: [PulseCode; MAX_PULSES] =
        [PulseCode::new(Level::Low, 0, Level::Low, 0); MAX_PULSES];

    info!("Starting BLE advertisement as Spect-Strip");
    loop {
        let mut adv_data_buf = [0u8; 31];
        let adv_len = AdStructure::encode_slice(
            &[
                AdStructure::Flags(0x06),
                AdStructure::CompleteLocalName(b"Spect-Strip"),
            ],
            &mut adv_data_buf,
        )
        .unwrap();
        let adv = Advertisement::ConnectableScannableUndirected {
            adv_data: &adv_data_buf[..adv_len],
            scan_data: &[],
        };
        let params = AdvertisementParameters::default();
        info!("Advertising...");
        match host.peripheral.advertise(&params, adv).await {
            Ok(advertiser) => {
                info!("Waiting for connection...");
                match advertiser.accept().await {
                    Ok(connection) => {
                        info!("Connection accepted from {:?}", connection.peer_address());
                        connection.set_bondable(true).ok();
                        match connection.with_attribute_server(server) {
                            Ok(gatt_connection) => {
                                info!("GATT connection established");
                                loop {
                                    match select(
                                        gatt_connection.next(),
                                        Timer::after(Duration::from_secs(60)),
                                    )
                                    .await
                                    {
                                        Either::First(event) => {
                                            match event {
                                                GattConnectionEvent::Disconnected { .. } => {
                                                    info!("Disconnected");
                                                    break;
                                                }
                                                GattConnectionEvent::PassKeyDisplay(_) => {
                                                    info!("PassKeyDisplay");
                                                }
                                                GattConnectionEvent::PassKeyConfirm(_) => {
                                                    info!("PassKeyConfirm");
                                                    gatt_connection.pass_key_confirm().ok();
                                                }
                                                GattConnectionEvent::PassKeyInput => {
                                                    info!("PassKeyInput requested");
                                                }
                                                GattConnectionEvent::PairingComplete { .. } => {
                                                    info!("PairingComplete");
                                                }
                                                GattConnectionEvent::PairingFailed(_) => {
                                                    info!("PairingFailed");
                                                }
                                                GattConnectionEvent::Gatt { event } => {
                                                    match event {
                                                        GattEvent::Write(write_event) => {
                                                            let handle = write_event.handle();
                                                            if handle == led_data_char.handle {
                                                                info!("LED_DATA write: {} bytes", write_event.data().len());
                                                                pixel_buf.extend_from_slice(write_event.data());
                                                            } else if handle == led_show_char.handle {
                                                                info!("LED_SHOW write: trigger");
                                                                let n_pixels =
                                                                    pixel_buf.len() / BYTES_PER_PIXEL;
                                                                info!("Showing {} pixels", n_pixels);
                                                                if !pixel_buf.is_empty() {
                                                                    flush_leds(&pixel_buf, &mut pulse_buf);
                                                                }
                                                                pixel_buf.clear();
                                                            }
                                                            if let Ok(reply) = write_event.accept() {
                                                                reply.send().await;
                                                            }
                                                        },
                                                        GattEvent::Read(read_event) => {
                                                            if let Ok(reply) = read_event.accept() {
                                                                reply.send().await;
                                                            }
                                                        },
                                                        _ => {}
                                                    }
                                                },
                                                _ => {}
                                            }
                                        },
                                        Either::Second(()) => {
                                            info!("Connection timeout (60s), restarting advertising");
                                            break;
                                        }
                                    }
                                }
                            },
                            Err(_e) => {}
                        }
                    }
                    Err(_e) => {}
                }
            }
            Err(_e) => {
                Timer::after(Duration::from_secs(1)).await;
            }
        }
    }
}

#[allow(clippy::large_stack_frames)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    rtt_target::rtt_init_defmt!();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    static mut RADIO_INIT: Option<esp_radio::Controller<'static>> = None;
    static mut RESOURCES: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    static mut STACK: Option<
        trouble_host::Stack<
            'static,
            ExternalController<BleConnector<'static>, 1>,
            DefaultPacketPool,
        >,
    > = None;
    static mut LED_VALUE_STORAGE: [u8; 1] = [0u8; 1];
    static mut SHOW_VALUE_STORAGE: [u8; 1] = [0u8; 1];
    static mut SERVER: Option<GattServer> = None;

    let led_data_char: Characteristic<[u8; 1]>;
    let led_show_char: Characteristic<[u8; 1]>;

    unsafe {
        RADIO_INIT = Some(esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller"));
        let radio_ref = (*core::ptr::addr_of!(RADIO_INIT)).as_ref().unwrap();
        let transport = BleConnector::new(radio_ref, peripherals.BT, Default::default()).unwrap();
        let ble_controller = ExternalController::<_, 1>::new(transport);
        let stack = trouble_host::new(
            ble_controller,
            &mut *core::ptr::addr_of_mut!(RESOURCES),
        ).set_io_capabilities(IoCapabilities::NoInputNoOutput);
        STACK = Some(stack);
        let stack = (*core::ptr::addr_of!(STACK)).as_ref().unwrap();

        let mut table: AttributeTable<'_, NoopRawMutex, MAX_ATTRIBUTES> = AttributeTable::new();
        let mut led_service = table.add_service(Service::new(LED_SERVICE_UUID));

        led_data_char = led_service
            .add_characteristic(
                LED_DATA_UUID,
                &[CharacteristicProp::WriteWithoutResponse],
                [0u8; 1],
                &mut *core::ptr::addr_of_mut!(LED_VALUE_STORAGE),
            )
            .build();

        led_show_char = led_service
            .add_characteristic(
                LED_SHOW_UUID,
                &[CharacteristicProp::WriteWithoutResponse],
                [0u8; 1],
                &mut *core::ptr::addr_of_mut!(SHOW_VALUE_STORAGE),
            )
            .build();

        let _service_handle = led_service.build();

        SERVER = Some(AttributeServer::new(table));
        let server = (*core::ptr::addr_of!(SERVER)).as_ref().unwrap();

        let rmt = core::mem::ManuallyDrop::new(
            esp_hal::rmt::Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap(),
        );
        let channel0 = core::ptr::read(&rmt.channel0);
        let channel = channel0
            .configure_tx(
                peripherals.GPIO21,
                TxChannelConfig::default()
                    .with_clk_divider(4)
                    .with_idle_output_level(Level::Low),
            )
            .unwrap();

        RMT_CHANNEL = Some(channel);

        unwrap!(spawner.spawn(ble_runner_task(stack)));
        unwrap!(spawner.spawn(gatt_server_task(
            stack,
            server,
            led_data_char,
            led_show_char
        )));
    }

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}