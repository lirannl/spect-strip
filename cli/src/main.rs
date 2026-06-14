use std::time::Duration;

use btleplug::api::{Central, Manager as _, Peripheral, ScanFilter, WriteType};
use btleplug::platform::Manager;
use clap::Parser;
use uuid::Uuid;

const LED_DATA_UUID: Uuid = Uuid::from_bytes([
    0xf0, 0xde, 0xbc, 0x9a, 0x78, 0x56, 0x34, 0x12, 0x78, 0x56, 0x34, 0x12, 0xde, 0xcc, 0xbb, 0xaa,
]);

const LED_SHOW_UUID: Uuid = Uuid::from_bytes([
    0xf0, 0xde, 0xbc, 0x9a, 0x78, 0x56, 0x34, 0x12, 0x78, 0x56, 0x34, 0x12, 0xdf, 0xcc, 0xbb, 0xaa,
]);

#[derive(Parser, Debug)]
#[command(name = "spect", about = "Control WS2812 LED strip via BLE")]
struct Args {
    #[arg(short, long, help = "3 bytes per pixel (6 hex chars), default 4 (RGBW, 8 hex chars)")]
    rgb: bool,
    #[arg(help = "BLE device address (xx:xx:xx:xx:xx:xx) or name prefix")]
    device: String,
    #[arg(help = "Hex color string per pixel", required = true, num_args = 1..)]
    colors: Vec<String>,
    #[arg(short, long, help = "Enable verbose logging")]
    verbose: bool,
}

fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err(format!("hex string must have even length: {}", s));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("invalid hex '{}': {}", s, e))
}

async fn find_device<C: Central>(central: &C, device_id: &str, verbose: bool) -> C::Peripheral {
    let is_addr = device_id.contains(':');

    loop {
        let peripherals = central.peripherals().await.unwrap();
        if verbose {
            println!("Found {} peripheral(s)", peripherals.len());
        }
        for p in &peripherals {
            let addr_str = p.address().to_string();
            if verbose {
                println!("  Device: {} (addr: {})", 
                    p.properties().await.ok().flatten().and_then(|p| p.local_name).unwrap_or_else(|| "unknown".to_string()),
                    addr_str
                );
            }
            if is_addr {
                if addr_str.starts_with(device_id) {
                    return p.clone();
                }
            } else if let Ok(Some(props)) = p.properties().await {
                if props
                    .local_name
                    .as_ref()
                    .is_some_and(|name| name.contains(device_id))
                {
                    return p.clone();
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let bpp = if args.rgb { 3 } else { 4 };
    let hex_len = bpp * 2;

    let mut raw = Vec::new();
    for s in &args.colors {
        if s.len() != hex_len {
            eprintln!("error: expected {hex_len} hex chars, got {}: {s}", s.len());
            std::process::exit(1);
        }
        match parse_hex(s) {
            Ok(b) => raw.extend(b),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    let n_pixels = raw.len() / bpp;
    println!(
        "Connecting to '{}'... ({n_pixels} pixel(s), {} bytes)",
        args.device,
        raw.len()
    );

    let manager = Manager::new().await.expect("BLE manager failed");
    let adapters = manager.adapters().await.expect("no adapters found");
    if adapters.is_empty() {
        eprintln!("No BLE adapters found");
        std::process::exit(1);
    }
    let central = adapters.into_iter().next().expect("no BLE adapter");
    if args.verbose {
        println!("Using adapter: {}", central.adapter_info().await?);
    }

    central
        .start_scan(ScanFilter::default())
        .await
        .expect("scan failed");
    println!("Scanning...");

    let device = find_device(&central, &args.device, args.verbose).await;
    central.stop_scan().await.ok();

    let addr = device.address();
    println!("Found: {addr}");

    if let Ok(Some(props)) = device.properties().await {
        if let Some(name) = props.local_name {
            println!("  Name: {name}");
        }
    }

    println!("Connecting...");
    device.connect().await.expect("connect failed");
    println!("Connected!");



    println!("Discovering services...");
    device
        .discover_services()
        .await
        .expect("service discovery failed");

    let chars = device.characteristics();
    if args.verbose {
        println!("Found {} characteristics:", chars.len());
        for c in &chars {
            println!("  UUID: {} (props: {:?})", c.uuid, c.properties);
        }
    }

    let data_char = chars
        .iter()
        .find(|c| c.uuid == LED_DATA_UUID)
        .unwrap_or_else(|| {
            eprintln!("LED_DATA characteristic not found (looking for {})", LED_DATA_UUID);
            std::process::exit(1);
        });

    let show_char = chars
        .iter()
        .find(|c| c.uuid == LED_SHOW_UUID)
        .unwrap_or_else(|| {
            eprintln!("LED_SHOW characteristic not found (looking for {})", LED_SHOW_UUID);
            std::process::exit(1);
        });

    println!("Sending pixel data...");
    device
        .write(data_char, &raw, WriteType::WithoutResponse)
        .await
        .expect("write failed");

    tokio::time::sleep(Duration::from_millis(50)).await;

    println!("Show!");
    device
        .write(show_char, &[1], WriteType::WithoutResponse)
        .await
        .expect("show failed");

    println!("Done!");
    device.disconnect().await.ok();

    Ok(())
}