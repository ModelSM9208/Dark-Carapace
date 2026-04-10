#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::select;
use embassy_time::Timer;
use esp_hal::clock::CpuClock;
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::controller::BleConnector;
use trouble_host::prelude::*;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    log::error!("PANIC: {}", info);
    loop {}
}

esp_bootloader_esp_idf::esp_app_desc!();

// ── GATT Server Definition ──────────────────────────────────────────
// trouble's #[gatt_server] macro generates the server struct, handles,
// and all ATT plumbing at compile time. No manual attribute arrays.

#[gatt_server]
struct Server {
    carapace_service: CarapaceService,
}

#[gatt_service(uuid = "937312e0-2354-11eb-9f10-fbc30a62cf38")]
struct CarapaceService {
    #[characteristic(uuid = "937312e0-2354-11eb-9f10-fbc30a62cf39", read, write, notify)]
    greeting: [u8; 23],
}

// ── BLE Runner Task ─────────────────────────────────────────────────
// This must run forever alongside the application. It processes HCI
// events from the radio controller.
async fn ble_task<C: Controller, P: PacketPool>(mut runner: Runner<'_, C, P>) {
    loop {
        if let Err(e) = runner.run().await {
            panic!("[ble_task] error: {:?}", e);
        }
    }
}

// ── GATT Event Handler ──────────────────────────────────────────────
// Processes read/write/disconnect events on the connection.
async fn gatt_events_task<P: PacketPool>(server: &Server<'_>, conn: &GattConnection<'_, '_, P>) {
    let greeting = server.carapace_service.greeting;
    loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { reason } => {
                log::info!("[gatt] disconnected: {:?}", reason);
                break;
            }
            GattConnectionEvent::Gatt { event } => {
                match &event {
                    GattEvent::Read(e) => {
                        if e.handle() == greeting.handle {
                            log::info!("[gatt] Read on greeting characteristic");
                        }
                    }
                    GattEvent::Write(e) => {
                        if e.handle() == greeting.handle {
                            log::info!("[gatt] Write on greeting: {:?}", e.data());
                        }
                    }
                    _ => {}
                }
                match event.accept() {
                    Ok(reply) => reply.send().await,
                    Err(e) => log::warn!("[gatt] error sending response: {:?}", e),
                }
            }
            _ => {}
        }
    }
}

// ── Notification Task ───────────────────────────────────────────────
// Sends periodic notifications to connected clients.
// Also reads RSSI to monitor signal strength.
async fn notify_task<P: PacketPool>(server: &Server<'_>, conn: &GattConnection<'_, '_, P>) {
    let greeting = server.carapace_service.greeting;
    let mut tick: u8 = 0;
    loop {
        tick = tick.wrapping_add(1);
        let msg = b"Hello from the Carapace";
        if greeting.notify(conn, msg).await.is_err() {
            log::info!("[notify] client gone, stopping notifications");
            break;
        }
        log::info!("[notify] tick={}", tick);
        Timer::after_secs(2).await;
    }
}

// ── Advertise and Accept ────────────────────────────────────────────
async fn advertise<'values, 'server, C: Controller>(
    peripheral: &mut Peripheral<'values, C, DefaultPacketPool>,
    server: &'server Server<'values>,
) -> Result<GattConnection<'values, 'server, DefaultPacketPool>, BleHostError<C::Error>> {
    let mut adv_data = [0; 31];
    let len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteLocalName(b"DarkCarapace"),
        ],
        &mut adv_data[..],
    )?;
    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &adv_data[..len],
                scan_data: &[],
            },
        )
        .await?;
    log::info!("[adv] advertising as DarkCarapace");
    let conn = advertiser.accept().await?.with_attribute_server(server)?;
    log::info!("[adv] connection established");
    Ok(conn)
}

// ── Main ────────────────────────────────────────────────────────────

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2; // Signal + ATT

#[allow(clippy::large_stack_frames, reason = "main allocates larger buffers")]
#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    esp_println::println!("=== DarkCarapace is alive ===");
    esp_println::logger::init_logger(log::LevelFilter::Info);
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 98768);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    // Hardware entropy source — feeds true randomness from silicon
    // TrngSource must stay alive for the lifetime of any Trng instances
    let _trng_source = TrngSource::new(peripherals.RNG, peripherals.ADC1);
    let rng = Trng::try_new().expect("Failed to create TRNG — TrngSource not active");

    // Initialize BLE radio controller
    let radio_init = esp_radio::init().expect("Failed to initialize BLE controller");
    let connector = BleConnector::new(&radio_init, peripherals.BT, Default::default())
        .expect("Failed to create BLE connector");
    let controller: ExternalController<_, 20> = ExternalController::new(connector);

    // Build the BLE host stack
    // The MAC address serves as our identity on the air — using hardware RNG for a random address
    let mut addr_bytes = [0u8; 6];
    rng.read(&mut addr_bytes);
    let address = Address::random(addr_bytes);
    log::info!("[init] BLE address: {:?}", address);

    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).set_random_address(address);
    let host = stack.build();
    let runner = host.runner;
    let mut peripheral = host.peripheral;

    // Initialize GATT server with our custom service
    let server = Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: "DarkCarapace",
        appearance: &appearance::UNKNOWN,
    }))
    .unwrap();

    // Set the initial greeting value
    let msg = b"Hello from the Carapace";
    server.set(&server.carapace_service.greeting, msg).unwrap();

    log::info!("[init] GATT server ready, entering main loop");

    // Run the BLE stack and application concurrently
    let _ = join(ble_task(runner), async {
        loop {
            match advertise(&mut peripheral, &server).await {
                Ok(conn) => {
                    let a = gatt_events_task(&server, &conn);
                    let b = notify_task(&server, &conn);
                    // Run until either task ends (disconnect), then re-advertise
                    select(a, b).await;
                    log::info!("[main] connection ended, re-advertising...");
                }
                Err(e) => {
                    panic!("[adv] error: {:?}", e);
                }
            }
        }
    })
    .await;

    // join never returns, but the compiler needs to see -> !
    loop {}
}
