#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
//use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::controller::BleConnector;

//ble tooth imports
use bleps::ad_structure::{
    AdStructure, BR_EDR_NOT_SUPPORTED, LE_GENERAL_DISCOVERABLE, create_advertising_data,
};
use bleps::async_attribute_server::AttributeServer;
use bleps::asynch::Ble;
//use bleps::attribute_server::WorkResult;
use bleps::gatt;
use esp_hal::rng::{Trng, TrngSource};

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    log::error!("PANIC: {}", info);
    loop {}
}

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]

fn millis() -> u64 {
    esp_hal::time::Instant::now()
        .duration_since_epoch()
        .as_millis()
}

#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    // generator version: 1.2.0

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    esp_println::println!("=== DarkCarapace is alive ===");
    esp_println::logger::init_logger(log::LevelFilter::Info);
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 98768);
    // COEX needs more RAM - so we've added some more
    //    esp_alloc::heap_allocator!(size: 64 * 1024); //Removed as we are only enabling bluetooth no wifi no coex

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    let radio_init = esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller");
    /*
        let (mut _wifi_controller, _interfaces) =
            esp_radio::wifi::new(&radio_init, peripherals.WIFI, Default::default())
                .expect("Failed to initialize Wi-Fi controller");
    */
    let connector = BleConnector::new(&radio_init, peripherals.BT, Default::default())
        .expect("Failed to create BLE connector");
    let mut ble = Ble::new(connector, millis);
    assert!(ble.init().await.is_ok(), "BLE init failed");
    log::info!("BLE init complete");

    // TrngSource must stay alive — it feeds hardware entropy to Trng.
    // Dropping it kills the entropy source and Trng will panic.
    let _trng_source = TrngSource::new(peripherals.RNG, peripherals.ADC1);
    let mut rng = Trng::try_new().expect("Failed to create TRNG — TrngSource not active");

    loop {
        // Advertise
        log::info!("(Re)starting advertising cycle...");
        ble.cmd_set_le_advertising_parameters()
            .await
            .expect("adv params failed");
        log::info!("Advertising params set");
        ble.cmd_set_le_advertising_data(
            create_advertising_data(&[
                AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
                AdStructure::CompleteLocalName("DarkCarapace"),
            ])
            .expect("adv data creation failed"),
        )
        .await
        .expect("adv data failed");
        log::info!("Advertising data set");
        ble.cmd_set_le_advertise_enable(true)
            .await
            .expect("adv enable failed");
        log::info!("Advertising enabled — scanning should find DarkCarapace");

        // GATT service with one readable characteristic
        let mut rf = |offset: usize, data: &mut [u8]| {
            let msg = b"Hello from the Carapace";
            if offset >= msg.len() {
                return 0;
            }
            let remaining = &msg[offset..];
            let len = remaining.len().min(data.len());
            data[..len].copy_from_slice(&remaining[..len]);
            len
        };
        let mut wf = |_offset: usize, _data: &[u8]| {};

        gatt!([service {
            uuid: "937312e0-2354-11eb-9f10-fbc30a62cf38",
            characteristics: [characteristic {
                uuid: "937312e0-2354-11eb-9f10-fbc30a62cf39",
                read: rf,
                write: wf,
            }],
        }]);

        let mut srv = AttributeServer::new(&mut ble, &mut gatt_attributes, &mut rng);
        let mut notifier = || core::future::pending();
        log::info!("GATT server running, waiting for connection...");
        srv.run(&mut notifier).await.expect("GATT server error");
        log::info!("Client disconnected or server exited, restarting...");

        // If we get here, client disconnected — loop back and re-advertise
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.0.0/examples
}
