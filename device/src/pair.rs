use alloc::boxed::Box;
use core::error::Error;
use trouble_host::prelude::*;
extern crate alloc;

pub async fn await_connection() -> Result<(), Box<dyn Error>> {
    let host = stack().build();
    let mut adv_data_buf = [0u8; 31];

    // Encode advertisement structures into the buffer
    // AdStructure::encode_slice() converts structured data to raw bytes
    // Returns: number of bytes written
    let adv_len = AdStructure::encode_slice(
        &[
            // Flags: Device capabilities and discovery mode
            // 0x06 = binary 0000_0110:
            //   Bit 1 (0x02): LE General Discoverable Mode
            //     - Device is always discoverable (not limited time)
            //     - Shows up in normal BLE scans
            //   Bit 2 (0x04): BR/EDR Not Supported
            //     - Only BLE, no classic Bluetooth
            //     - ESP32-C6 supports both, but we disable classic
            AdStructure::Flags(0x06),
            // Complete Local Name: Human-readable device name
            // This is what shows up in Bluetooth settings on phones
            // Changed to "ESP32-Counter" to indicate this is the counter demo
            AdStructure::CompleteLocalName(b"ESP32-Counter"),
        ],
        &mut adv_data_buf, // Write encoded data here
    )
    .unwrap(); // Panic if encoding fails (shouldn't happen with this data)

    // ====================================================================
    // STEP 2: Configure Advertisement Type
    // ====================================================================

    // Create the advertisement with encoded data
    // ConnectableScannableUndirected means:
    // - Connectable: Devices can connect to us (peripheral mode)
    // - Scannable: Devices can request more info (scan response)
    // - Undirected: Broadcasting to everyone (not a specific device)
    let adv = Advertisement::ConnectableScannableUndirected {
        // Primary advertisement data (what's always sent)
        adv_data: &adv_data_buf[..adv_len], // Use only filled portion

        // Scan response data (sent when requested)
        // Could include: service UUIDs, manufacturer data, etc.
        // Empty for now - the name and flags are enough
        scan_data: &[],
    };

    // ====================================================================
    // STEP 3: Set Advertisement Parameters
    // ====================================================================

    // Configure timing and behavior
    // Default parameters:
    // - interval: 160ms (balance between discovery speed and power)
    // - tx_power: 0 dBm (medium range, ~10 meters)
    // - channels: All three advertising channels (37, 38, 39)
    let params = AdvertisementParameters::default();

    // ====================================================================
    // STEP 4: Start Advertising
    // ====================================================================

    // host.peripheral.advertise() is async - it:
    // 1. Configures the BLE controller for advertising
    // 2. Starts broadcasting advertisement packets
    // 3. Returns an Advertiser handle immediately
    // The actual broadcasting happens in the background
    //
    // NOTE: This only works because ble_runner_task is running concurrently,
    // processing HCI events. Without the runner, this would hang forever.
    match host.peripheral.advertise(&params, adv).await {
        // Success: We have an Advertiser handle
        Ok(advertiser) => {
            info!("Advertising started, waiting for connection...")
        }
    }
}
