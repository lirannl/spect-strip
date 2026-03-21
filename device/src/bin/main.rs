// ============================================================================
// CRATE ATTRIBUTES - Configure how this Rust crate behaves
// ============================================================================

// #![no_std] - Don't use Rust's standard library
// Why: ESP32-C6 is a bare-metal embedded system with no operating system.
// The standard library requires OS features like files, threads, heap allocation
// that don't exist here. We use 'core' (minimal Rust) + embedded-specific crates.
#![no_std]
// #![no_main] - Don't use Rust's standard main() entry point
// Why: Normal Rust programs start with fn main() called by the OS.
// Embedded systems have their own startup code. The esp_rtos::main macro
// creates the real entry point that the ESP32 bootloader calls.
#![no_main]
// Deny using mem::forget with ESP HAL types
// Why: ESP HAL types often hold hardware resources (DMA buffers, radio state).
// Using mem::forget would leak these resources and cause hardware issues.
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
// Warn if functions use too much stack space
// Why: Embedded systems have limited RAM (512KB on ESP32-C6). Large stack
// frames can cause stack overflow crashes. This catches functions using >4KB.
#![deny(clippy::large_stack_frames)]

// ============================================================================
// IMPORTS - Bring in functionality from external crates
// ============================================================================

// ExternalController: Wraps BLE transport layer in HCI (Host Controller Interface)
// HCI is the standard protocol between BLE host (software) and controller (hardware)
use bt_hci::controller::ExternalController;

// defmt: Efficient logging for embedded systems
// - info!(): Log informational messages
// - unwrap!(): Like .unwrap() but works with defmt logging on panic
// Messages are sent via RTT (Real-Time Transfer) over the debug probe
use defmt::{info, unwrap};

// Spawner: Creates and manages async tasks in Embassy
// Embassy is our async runtime - like tokio but for embedded systems
use embassy_executor::Spawner;

// NoopRawMutex: A mutex that does nothing (no actual locking)
// Why: On single-core ESP32-C6 with cooperative async, we don't need real locking
// The GATT AttributeTable requires a mutex type, but since we're single-threaded
// and cooperative (not preemptive), NoopRawMutex is safe and efficient
use embassy_sync::blocking_mutex::raw::NoopRawMutex;

// select: Run two futures concurrently, return when first completes
// Either: Enum indicating which future completed (First or Second)
// Used to handle GATT events OR timer expiration, whichever happens first
use embassy_futures::select::{Either, select};

// Duration/Timer: Async time primitives
// - Duration: Represents time spans (e.g., 1 second)
// - Timer::after(): Async delay - yields control while waiting
use embassy_time::{Duration, Timer};

// CpuClock: Configure ESP32-C6 CPU frequency
// Can run at different speeds: 80MHz, 160MHz (default), or 240MHz
// Higher = faster but more power consumption
use esp_hal::clock::CpuClock;

// TimerGroup: Hardware timer peripheral
// ESP32-C6 has two timer groups (TIMG0, TIMG1), each with timers
// Embassy uses one for task scheduling and time tracking
use esp_hal::timer::timg::TimerGroup;

// BleConnector: Connects ESP32's radio hardware to the BLE stack
// Provides HCI transport layer using ESP32's shared WiFi/BLE radio
use esp_radio::ble::controller::BleConnector;

// trouble_host::prelude: Brings in BLE host stack types
// Includes: Stack, HostResources, Advertisement, AdStructure, etc.
// The "prelude" contains commonly-used types from the crate
use trouble_host::prelude::*;

// ============================================================================
// PANIC HANDLER - What happens when the program panics (fatal error)
// ============================================================================

// In no_std environments, you MUST define what happens on panic
// This is called when: unwrap() fails, array out of bounds, division by zero, etc.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // On panic, enter infinite loop
    // The '-> !' means "never returns" - function runs forever
    // Why infinite loop? No OS to crash to, no way to recover.
    // In a real system you might:
    // - Blink an LED pattern to signal error
    // - Log error details via defmt
    // - Reset the device
    loop {}
}

// ============================================================================
// ALLOCATOR SETUP
// ============================================================================

// Enable the 'alloc' crate for dynamic memory allocation
// Provides: Vec, String, Box, Arc, etc.
// Note: We configure the actual allocator later in main() using esp_alloc
extern crate alloc;

// ============================================================================
// CONSTANTS - Configuration values used throughout the program
// ============================================================================

// Maximum number of simultaneous BLE connections
// ESP32-C6 can theoretically handle more, but each connection uses RAM
// For this example: 1 connection is enough (typical peripheral use case)
const CONNECTIONS_MAX: usize = 1;

// Maximum number of L2CAP channels per connection
// L2CAP (Logical Link Control and Adaptation Protocol) multiplexes data
// Each GATT service/characteristic uses a channel
// 2 channels: one for ATT (GATT), one for signaling
const L2CAP_CHANNELS_MAX: usize = 2;

// Maximum attributes in our GATT table
// In BLE/GATT, everything is an "attribute":
// - Service declarations
// - Characteristic declarations
// - Characteristic values
// - Descriptors (like CCCD for notifications)
// Our counter service needs roughly:
// - 1 service declaration
// - 1 characteristic declaration
// - 1 characteristic value
// - 1 CCCD (Client Characteristic Configuration Descriptor)
// 16 is plenty of room for expansion
const MAX_ATTRIBUTES: usize = 16;

// ============================================================================
// GATT SERVICE UUIDS - Unique identifiers for our BLE services
// ============================================================================

// What are UUIDs?
// UUIDs (Universally Unique Identifiers) are 128-bit numbers that identify
// services and characteristics in BLE. There are two types:
// - Standard UUIDs: Defined by Bluetooth SIG (e.g., Heart Rate = 0x180D)
// - Custom UUIDs: 128-bit UUIDs you create for your own services
//
// We use custom UUIDs since we're creating our own counter service.
// Format: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx (as bytes in little-endian)
// You can generate your own at: https://www.uuidgenerator.net/

// Custom Service UUID for our Counter Service
// This identifies the service when a phone scans for it
// In nRF Connect, you'll see this UUID listed under the device
const COUNTER_SERVICE_UUID: Uuid = Uuid::new_long([
    0x12, 0x34, 0x56, 0x78, // First 4 bytes
    0x12, 0x34, // Next 2 bytes
    0x12, 0x34, // Next 2 bytes
    0x12, 0x34, // Next 2 bytes
    0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, // Last 6 bytes
]);

// Characteristic UUID for Counter Value
// This identifies the specific data point (the counter) within the service
// Must be different from the service UUID
const COUNTER_CHAR_UUID: Uuid = Uuid::new_long([
    0x12, 0x34, 0x56, 0x79, // Note: last byte is 0x79, not 0x78
    0x12, 0x34, 0x12, 0x34, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0,
]);

// ============================================================================
// BOOTLOADER METADATA
// ============================================================================

// Create ESP-IDF bootloader app descriptor
// This macro generates metadata the ESP32 bootloader reads on startup:
// - App name, version, compile time
// - Flash configuration
// - Security version (for secure boot)
// Required for ESP-IDF compatibility
esp_bootloader_esp_idf::esp_app_desc!();

// ============================================================================
// ADVERTISING TASK - Async task that handles BLE advertising
// ============================================================================

// Type alias for our BLE stack to avoid repeating long generic types
// ExternalController<BleConnector<'static>, 1>: HCI controller with max 1 concurrent command
// DefaultPacketPool: Pre-allocated packet buffer pool from trouble_host
type BleStack =
    trouble_host::Stack<'static, ExternalController<BleConnector<'static>, 1>, DefaultPacketPool>;

// Type alias for our GATT Attribute Server
// The AttributeServer handles all GATT protocol operations:
// - Responding to read requests from phones
// - Processing write requests
// - Managing notification subscriptions (CCCD)
// Generic parameters:
// - 'static: Lives for entire program
// - NoopRawMutex: No-op mutex (safe on single-core cooperative async)
// - DefaultPacketPool: Packet buffer pool
// - MAX_ATTRIBUTES: Maximum number of attributes in our table
// - 1: Maximum CCCDs (notification descriptors) - we have 1 for our counter
// - CONNECTIONS_MAX: Maximum simultaneous connections
type GattServer = AttributeServer<
    'static,
    NoopRawMutex,
    DefaultPacketPool,
    MAX_ATTRIBUTES,
    1, // Max CCCDs
    CONNECTIONS_MAX,
>;

// ============================================================================
// RUNNER TASK - Background task that processes BLE HCI events
// ============================================================================

// CRITICAL: This task MUST be running for BLE to work!
// The runner processes HCI (Host Controller Interface) events from the ESP32
// radio hardware. Without it, advertise(), accept(), and all other BLE
// operations will hang forever waiting for responses that never get processed.
#[embassy_executor::task]
async fn ble_runner_task(stack: &'static BleStack) {
    info!("BLE runner starting...");

    // Build the host and extract just the runner
    // The Host struct contains: peripheral, central, and runner
    // We only need the runner here - peripheral is used by advertise_task
    let host = stack.build();
    let mut runner = host.runner;

    // Run the HCI event loop forever
    // This processes all BLE events: connection complete, data received,
    // advertising terminated, etc.
    if let Err(_e) = runner.run().await {
        info!("BLE runner error!");
    }

    info!("BLE runner exited");
}

// ============================================================================
// GATT SERVER TASK - Handles BLE advertising, connections, and GATT services
// ============================================================================

// #[embassy_executor::task] - Mark this as an Embassy async task
// Embassy tasks are like threads but cooperative (not preemptive)
// They run concurrently by yielding at .await points
#[embassy_executor::task]
async fn gatt_server_task(
    // Parameter: Reference to the BLE stack (lives forever - 'static)
    // Stack is generic over:
    // - Controller type: ExternalController wrapping BleConnector
    // - Packet pool type: DefaultPacketPool for BLE packet buffers
    stack: &'static BleStack,

    // Parameter: Reference to our GATT server
    // The server holds the attribute table (services/characteristics)
    // and handles GATT protocol operations
    server: &'static GattServer,

    // Parameter: Handle to our counter characteristic
    // We need this to:
    // - Update the counter value in the attribute table
    // - Send notifications to connected devices
    // Characteristic<u32> means this characteristic holds a u32 value
    counter_char: Characteristic<u32>,
) {
    // Small delay to let the runner task start first
    // The runner must be processing events before we try to advertise
    Timer::after(Duration::from_millis(100)).await;

    // Build the host from the stack
    // stack.build() consumes the Stack and produces a Host with three components:
    // - peripheral: For advertising and accepting connections
    // - central: For scanning and initiating connections (not used here)
    // - runner: Background task that processes BLE events (handled by ble_runner_task)
    let mut host = stack.build();

    // Initialize our counter
    // This value will increment every second and be sent to connected phones
    let mut counter: u32 = 0;

    // Main advertising loop - runs forever
    loop {
        info!("Starting BLE advertising...");

        // ====================================================================
        // STEP 1: Create Advertisement Data
        // ====================================================================

        // BLE advertisements are small packets (max 31 bytes) broadcast periodically
        // They contain "AD Structures" - type-length-value encoded data

        // Buffer to hold encoded advertisement data (31 bytes = BLE 4.x max)
        // BLE 5.0 allows larger extended advertisements, but we use legacy mode
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
                info!("Advertising started, waiting for connection...");

                // ============================================================
                // STEP 5: Wait for Connection
                // ============================================================

                // advertiser.accept() is async - blocks until:
                // - A central device (phone, computer) connects to us
                // - An error occurs (timeout, hardware failure)
                match advertiser.accept().await {
                    // Connection established!
                    Ok(connection) => {
                        info!("Device connected!");

                        // ====================================================
                        // STEP 6: Create GATT Connection
                        // ====================================================

                        // GattConnection wraps the raw BLE connection with our GATT server
                        // This enables:
                        // - Handling read/write requests from the phone
                        // - Sending notifications when data changes
                        // - Automatic GATT protocol handling
                        //
                        // with_attribute_server() registers this connection with the server's CCCD table
                        // (CCCD = Client Characteristic Configuration Descriptor)
                        // The CCCD tracks which characteristics the client wants notifications for
                        let gatt_connection = match connection.with_attribute_server(server) {
                            Ok(conn) => conn,
                            Err(_e) => {
                                info!("Failed to create GATT connection");
                                continue; // Go back to advertising
                            }
                        };

                        info!("GATT connection established!");
                        info!("Open nRF Connect on your phone:");
                        info!("  1. Connect to 'ESP32-Counter'");
                        info!("  2. Expand the custom service (UUID starting with 12345678...)");
                        info!("  3. Tap the three arrows icon to enable notifications");
                        info!("  4. Watch the counter value update every second!");

                        // ====================================================
                        // STEP 7: Connection Event Loop
                        // ====================================================

                        // While connected, we need to:
                        // 1. Handle incoming GATT events (reads, writes from phone)
                        // 2. Update counter and send notifications every second
                        //
                        // We use select() to wait for EITHER:
                        // - A GATT event from the phone, OR
                        // - Our 1-second timer firing
                        // Whichever happens first gets processed

                        loop {
                            match select(
                                gatt_connection.next(),               // Wait for GATT event
                                Timer::after(Duration::from_secs(1)), // Wait 1 second
                            )
                            .await
                            {
                                // ============================================
                                // Case A: GATT Event Received
                                // ============================================
                                Either::First(event) => {
                                    match event {
                                        // Phone disconnected
                                        GattConnectionEvent::Disconnected { reason } => {
                                            info!("Disconnected: {:?}", reason);
                                            break; // Exit loop, go back to advertising
                                        }

                                        // GATT read/write request from phone
                                        GattConnectionEvent::Gatt { event } => {
                                            // accept() processes the request:
                                            // - For reads: returns the characteristic value
                                            // - For writes: updates the characteristic value
                                            // The server handles all the GATT protocol details
                                            match event.accept() {
                                                Ok(reply) => {
                                                    // Send response back to phone
                                                    // send() consumes the reply and sends it
                                                    reply.send().await;
                                                }
                                                Err(_e) => {
                                                    info!("Failed to accept GATT event");
                                                }
                                            }
                                        }

                                        // Other events (PHY update, connection params, etc.)
                                        // We ignore these for simplicity
                                        _ => {}
                                    }
                                }

                                // ============================================
                                // Case B: Timer Fired - Update Counter
                                // ============================================
                                Either::Second(()) => {
                                    // Increment counter (wrapping at u32::MAX)
                                    counter = counter.wrapping_add(1);
                                    info!("Counter: {}", counter);

                                    // Send notification to phone with new value
                                    // notify() does two things:
                                    // 1. Updates the value in the attribute table
                                    // 2. Sends a notification packet to the phone
                                    //
                                    // Note: Notification only works if the phone has enabled
                                    // notifications for this characteristic (via CCCD)
                                    // If not enabled, notify() succeeds but nothing is sent
                                    if let Err(_e) =
                                        counter_char.notify(&gatt_connection, &counter).await
                                    {
                                        // This can fail if:
                                        // - Client hasn't subscribed to notifications (normal)
                                        // - Connection was lost
                                        // We just continue - it's not an error
                                    }
                                }
                            }
                        }
                    }

                    // Connection accept failed
                    // Could happen if: central disconnects during pairing,
                    // timeout expires, or hardware error
                    Err(_e) => {
                        info!("Connection accept failed");
                        // Loop will restart advertising automatically
                    }
                }
            }

            // Advertising failed to start
            // Could happen if: hardware not ready, invalid parameters,
            // or already advertising
            Err(_e) => {
                info!("Advertising failed");

                // Wait 1 second before retrying
                // Prevents rapid retry loop if there's a persistent error
                Timer::after(Duration::from_secs(1)).await;
            }
        }

        // Loop repeats: start advertising again
        // This makes the device always discoverable and connectable
    }
}

// ============================================================================
// MAIN FUNCTION - Entry point of the program
// ============================================================================

// Allow large stack frames in main
// Main function does a lot of initialization which needs stack space
// This is acceptable since it only happens once at startup
#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
// #[esp_rtos::main] - Transform this into ESP32 entry point
// This macro:
// 1. Creates the real entry point the ESP32 bootloader calls
// 2. Sets up Embassy executor for async tasks
// 3. Configures timers and interrupts for task scheduling
// 4. Runs this function inside the executor
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // spawner: Used to create new async tasks
    // -> ! : Function never returns (runs forever)

    // Comment left by esp-generate showing template version
    // generator version: 1.2.0

    // ========================================================================
    // STEP 1: Initialize RTT Logging
    // ========================================================================

    // RTT (Real-Time Transfer): Fast logging via debug probe
    // Advantages over UART:
    // - No pins needed (uses JTAG)
    // - Very fast (doesn't block CPU)
    // - Bidirectional (can send input to device)
    // This macro initializes RTT with defmt support
    rtt_target::rtt_init_defmt!();

    // ========================================================================
    // STEP 2: Initialize ESP32-C6 Hardware
    // ========================================================================

    // Create default HAL configuration
    // Then override CPU clock to maximum speed
    // ESP32-C6 can run at: 80MHz, 160MHz (max)
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());

    // Initialize the HAL with this configuration
    // This:
    // - Sets up CPU clock
    // - Initializes peripheral drivers
    // - Returns handles to all hardware peripherals (GPIO, timers, radio, etc.)
    let peripherals = esp_hal::init(config);

    // ========================================================================
    // STEP 3: Set Up Heap Allocator
    // ========================================================================

    // Configure heap for dynamic memory allocation (Vec, Box, etc.)
    // Parameters:
    // - #[esp_hal::ram(reclaimed)]: Use RAM freed from bootloader
    //   Bootloader uses RAM during startup, we can reclaim it after
    // - size: 65536 bytes (64KB) for heap
    // ESP32-C6 has 512KB RAM total, 64KB heap is reasonable for BLE
    // Remaining RAM used for: stack, static variables, DMA buffers
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);

    // ========================================================================
    // STEP 4: Configure Embassy Async Runtime
    // ========================================================================

    // Get hardware timer group 0
    // ESP32-C6 has two timer groups (TIMG0, TIMG1)
    // Each group has multiple timers
    let timg0 = TimerGroup::new(peripherals.TIMG0);

    // Create software interrupt controller
    // Software interrupts are used for task switching in Embassy
    // The async executor uses these to schedule tasks
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);

    // Start the Embassy executor
    // This configures:
    // - timg0.timer0: Used as time source for embassy_time
    // - sw_interrupt.software_interrupt0: Used for task wake-ups
    // After this call, .await works and tasks can be spawned
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    info!("Embassy initialized!");

    // ========================================================================
    // STEP 5: Initialize BLE Stack
    // ========================================================================

    // We need static storage for BLE resources because:
    // 1. The BLE stack must live for the entire program ('static lifetime)
    // 2. Async tasks need 'static references
    // 3. Can't use local variables (would be dropped when main continues)

    // Static mutable storage for radio controller
    // Option<T> because we initialize it later (starts as None)
    // Must be mutable because we set it once, read it many times
    static mut RADIO_INIT: Option<esp_radio::Controller<'static>> = None;

    // Static mutable storage for BLE host resources
    // HostResources contains:
    // - Packet buffers for sending/receiving BLE data
    // - Connection state for each active connection
    // - L2CAP channel state
    // Generic parameters:
    // - DefaultPacketPool: Pre-allocated packet buffer pool
    // - CONNECTIONS_MAX (1): Max simultaneous connections
    // - L2CAP_CHANNELS_MAX (1): Max channels per connection
    static mut RESOURCES: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();

    // Static mutable storage for the BLE stack itself
    // Stack is the main entry point for all BLE operations
    // Generic parameters must match our specific types:
    // - ExternalController<BleConnector<'static>, 1>: Our controller type
    // - DefaultPacketPool: Our packet pool type
    static mut STACK: Option<
        trouble_host::Stack<
            'static,
            ExternalController<BleConnector<'static>, 1>,
            DefaultPacketPool,
        >,
    > = None;

    // ========================================================================
    // STEP 6: Initialize GATT Attribute Table
    // ========================================================================

    // What is GATT?
    // GATT (Generic Attribute Profile) is how BLE devices expose data.
    // It defines a hierarchy:
    // - Server: The device exposing data (our ESP32)
    // - Service: A collection of related data (e.g., "Counter Service")
    // - Characteristic: A single data point (e.g., "Counter Value")
    // - Descriptor: Metadata about a characteristic (e.g., CCCD for notifications)
    //
    // The AttributeTable stores all our services and characteristics.
    // Must be static because the GATT server references it for the entire program.

    // Static storage for the counter characteristic's value
    // Why a separate buffer? The attribute table stores references to data,
    // not the data itself. This buffer holds the actual counter bytes.
    // u32 = 4 bytes, stored in little-endian format
    static mut COUNTER_VALUE_STORAGE: [u8; 4] = [0u8; 4];

    // Static storage for the GATT server
    // The server wraps the attribute table and handles GATT protocol
    // We use Option because we can't initialize it at compile time
    // (AttributeTable::new() is not const)
    static mut SERVER: Option<GattServer> = None;

    // ========================================================================
    // STEP 7: Initialize BLE Controller and Stack (unsafe block)
    // ========================================================================

    // Why unsafe?
    // - Accessing mutable static variables is unsafe in Rust
    // - Multiple references to mutable statics can cause data races
    // - We're careful: only access from main thread during init
    // - After init, only immutable references are used

    // Variable to hold our characteristic handle
    // Set during initialization, used by the GATT server task
    let counter_char: Characteristic<u32>;

    unsafe {
        // ----------------------------------------------------------------
        // 7a. Initialize Radio Hardware
        // ----------------------------------------------------------------

        // esp_radio::init() initializes the shared WiFi/BLE radio
        // ESP32-C6 has one radio that can do WiFi OR BLE or both time-sliced
        // This:
        // - Powers on the radio
        // - Loads PHY calibration data from flash
        // - Configures RF parameters
        // Returns: Controller handle to access the radio
        RADIO_INIT = Some(esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller"));

        // Get reference to the controller
        // Why core::ptr::addr_of!?
        // - Rust 2024 edition forbids direct references to mutable statics
        // - addr_of! creates a raw pointer, then we convert to reference
        // - This is safe because we're not mutating while reading
        let radio_ref = (*core::ptr::addr_of!(RADIO_INIT)).as_ref().unwrap();

        // ----------------------------------------------------------------
        // 7b. Create BLE Transport Layer
        // ----------------------------------------------------------------

        // BleConnector wraps the radio controller for BLE-specific operations
        // Parameters:
        // - radio_ref: Reference to radio controller
        // - peripherals.BT: BLE peripheral hardware interface
        // - Default::default(): Use default BLE configuration
        // Returns: Transport that speaks HCI protocol to the radio
        let transport = BleConnector::new(radio_ref, peripherals.BT, Default::default()).unwrap();

        // ----------------------------------------------------------------
        // 7c. Wrap Transport in HCI Controller
        // ----------------------------------------------------------------

        // ExternalController implements the HCI (Host Controller Interface)
        // HCI is the standard protocol between:
        // - Host: Software (our Trouble stack)
        // - Controller: Hardware (ESP32 radio)
        // The <_, 1> means:
        // - _: Infer the transport type
        // - 1: Max 1 concurrent HCI command (simple, uses less RAM)
        let ble_controller = ExternalController::<_, 1>::new(transport);

        // ----------------------------------------------------------------
        // 7d. Create the BLE Host Stack
        // ----------------------------------------------------------------

        // trouble_host::new() creates the BLE stack
        // This brings together:
        // - ble_controller: Talks to hardware
        // - RESOURCES: Provides packet buffers and state storage
        // Returns: Stack with peripheral, central, and runner
        //
        // Why &mut *core::ptr::addr_of_mut!?
        // - addr_of_mut! gets mutable raw pointer to RESOURCES
        // - * dereferences the raw pointer
        // - &mut creates a mutable reference
        // - Required because Rust 2024 forbids direct &mut static_mut
        STACK = Some(trouble_host::new(
            ble_controller,
            &mut *core::ptr::addr_of_mut!(RESOURCES),
        ));

        // Get immutable reference to the stack
        // This reference lives forever ('static) and is safe to share
        // with the GATT server task
        let stack = (*core::ptr::addr_of!(STACK)).as_ref().unwrap();

        // ----------------------------------------------------------------
        // 7e. Build GATT Attribute Table
        // ----------------------------------------------------------------

        info!("Building GATT attribute table...");

        // Create the attribute table
        // This holds all our services, characteristics, and descriptors
        // We create it locally, build it up, then move it into the AttributeServer
        let mut table: AttributeTable<'_, NoopRawMutex, MAX_ATTRIBUTES> = AttributeTable::new();

        // Add our Counter Service to the table
        // A service groups related characteristics together
        // Think of it like a folder containing related files
        let mut counter_service = table.add_service(Service::new(COUNTER_SERVICE_UUID));

        // Add the Counter Characteristic to the service
        // This is the actual data point - the counter value
        //
        // Parameters:
        // - COUNTER_CHAR_UUID: Unique identifier for this characteristic
        // - &[CharacteristicProp::Read, CharacteristicProp::Notify]: Properties
        //   - Read: Phone can read the current value
        //   - Notify: ESP32 can push updates to phone without phone asking
        // - 0u32: Initial value (counter starts at 0)
        // - &mut COUNTER_VALUE_STORAGE: Buffer to store the value bytes
        //
        // Returns: CharacteristicBuilder which we call .build() on to get the handle
        counter_char = counter_service
            .add_characteristic(
                COUNTER_CHAR_UUID,
                &[CharacteristicProp::Read, CharacteristicProp::Notify],
                0u32, // Initial value
                &mut *core::ptr::addr_of_mut!(COUNTER_VALUE_STORAGE),
            )
            .build();

        // Finish building the service
        // This finalizes all the attribute handles
        let _service_handle = counter_service.build();

        info!("GATT table built!");
        info!("  Counter characteristic handle: {}", counter_char.handle);

        // ----------------------------------------------------------------
        // 7f. Create GATT Server
        // ----------------------------------------------------------------

        // The AttributeServer wraps the table and handles GATT protocol
        // It processes read/write requests and manages notifications
        // AttributeServer::new() takes ownership of the table
        SERVER = Some(AttributeServer::new(table));
        let server = (*core::ptr::addr_of!(SERVER)).as_ref().unwrap();

        // ----------------------------------------------------------------
        // 7g. Spawn BLE Tasks
        // ----------------------------------------------------------------

        // IMPORTANT: We spawn TWO tasks that work together:
        // 1. ble_runner_task: Processes HCI events from the radio hardware
        // 2. gatt_server_task: Handles advertising, connections, and GATT
        //
        // The runner MUST be running for any BLE operations to work.
        // Without it, advertise() and other calls hang waiting for
        // HCI responses that never get processed.

        // spawner.spawn() creates a new async task
        // - unwrap!: Assert the task spawns successfully (should always work)
        // The tasks start running immediately, managed by Embassy

        // Spawn the runner first - it needs to be processing events
        // before we try to advertise
        unwrap!(spawner.spawn(ble_runner_task(stack)));

        // Spawn the GATT server task with our server and counter characteristic
        unwrap!(spawner.spawn(gatt_server_task(stack, server, counter_char)));
    }

    // ========================================================================
    // STEP 8: Main Loop
    // ========================================================================

    // Main continues running while the BLE tasks run concurrently
    // This shows Embassy's cooperative multitasking in action:
    // - main() runs this loop (just sleeping, doing nothing)
    // - ble_runner_task() processes HCI events
    // - gatt_server_task() handles advertising, connections, and GATT
    // - All yield at .await points, letting each other run
    loop {
        // Main has nothing to do - all work is in the spawned tasks
        // We just sleep forever, yielding to the BLE tasks
        Timer::after(Duration::from_secs(60)).await;
    }

    // Note: This function never reaches here due to '-> !' return type
    // The loop above runs forever (or until panic/reset)

    // For more examples and inspiration:
    // https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.0.0/examples
}
