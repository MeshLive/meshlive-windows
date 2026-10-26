//! Root-approved on-air check (run only with `--approved`). No pairing, no
//! connection is ever initiated, no device name is advertised, radio state is
//! never changed. Three steps, about 2.5 minutes:
//!   1. active scan, 30 s: prints every decoded MeshLive announcement;
//!   2. connectable GattServiceProvider advertising with the current UUIDs, 60 s;
//!   3. step 2 plus a non-connectable BluetoothLEAdvertisementPublisher set with
//!      the 0xFFFF announcement and the c8d5a101 UUID, 60 s.
//! The announcement uses a fresh random 6-byte prefix labelled as a test; it
//! is not a MeshLive identity. Windows does not expose the random advertising
//! address of either API; the adapter's public address is printed instead and
//! the observing phone reports the on-air addresses.
use meshlive::{
    bluetooth::{
        peripheral::Peripheral,
        probe,
        scan::{CONTROL_SERVICE, ScanOptions, Scanner},
    },
    crypto,
    discovery::{ACCEPTING, Announcement, CALL, GATT, STREAM},
};
use std::{
    collections::HashMap,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    Devices::Bluetooth::{Advertisement::*, BluetoothAdapter},
    Foundation::TypedEventHandler,
    Storage::Streams::DataWriter,
    Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize},
    core::GUID,
};

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn kind(a: &Announcement) -> &'static str {
    if a.flags & STREAM != 0 {
        "stream_offer"
    } else if a.flags & CALL != 0 {
        "call_offer"
    } else if a.flags & ACCEPTING != 0 {
        "viewer_request"
    } else {
        "other"
    }
}
fn log(step: u8, event: &str, detail: &str) {
    println!("t={} step={step} event={event} {detail}", now_ms());
}

fn step1() {
    log(
        1,
        "start",
        "mode=active duration_s=30 rotating=true compact=true",
    );
    let scanner = match Scanner::start(ScanOptions {
        rotating: true,
        compact: true,
    }) {
        Ok(s) => s,
        Err(e) => return log(1, "error", &format!("start={e:?}")),
    };
    let cancelled = AtomicBool::new(false);
    let end = Instant::now() + Duration::from_secs(30);
    let mut last: HashMap<(u64, Vec<u8>), Instant> = HashMap::new();
    let mut count = 0_u32;
    while Instant::now() < end {
        match scanner.receive(end, &cancelled) {
            Ok(s) => {
                let (address, kind_of_address) = s.route.diagnostic_address();
                let raw = s
                    .announcement
                    .encode()
                    .map(|r| r.to_vec())
                    .unwrap_or_default();
                // At most one line per address and announcement per second.
                let key = (address, raw.clone());
                if last
                    .get(&key)
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
                {
                    continue;
                }
                last.insert(key, Instant::now());
                count += 1;
                let a = &s.announcement;
                log(
                    1,
                    "announcement",
                    &format!(
                        "address={address:012x} address_type={kind_of_address} rssi={} version={} kind={} id={} flags={:02x} summary={:02x} token={:08x} cost={} raw={}",
                        s.rssi,
                        a.version,
                        kind(a),
                        hex(&a.prefix),
                        a.flags,
                        a.summary,
                        a.token,
                        a.cost,
                        hex(&raw)
                    ),
                );
            }
            Err(meshlive::bluetooth::Error::Timeout) => break,
            Err(e) => {
                log(1, "error", &format!("receive={e:?}"));
                break;
            }
        }
    }
    drop(scanner);
    log(1, "end", &format!("lines={count}"));
}

fn test_offer() -> Announcement {
    let random = crypto::random_nonce().expect("CNG random");
    Announcement {
        version: 2,
        prefix: random[..6].try_into().unwrap(),
        flags: ACCEPTING | CALL | GATT,
        summary: 0x01,
        token: 0,
        cost: 0,
        sequence: 0,
        width: 0,
        height: 0,
        fps: 0,
        bitrate: 0,
        recovery: None,
    }
}

fn advertise(step: u8, offer: &Announcement, publisher: bool) {
    let raw = offer.encode().unwrap();
    let compact = offer.compact().unwrap();
    log(
        step,
        "start",
        &format!(
            "duration_s=60 test_prefix={} gatt_services=c8d5a105,{} discoverable=false publisher={publisher}",
            hex(&offer.prefix),
            hex(&compact)
        ),
    );
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut peripheral = match Peripheral::create(offer, cancelled.clone()) {
        Ok(p) => p,
        Err(e) => return log(step, "error", &format!("create={e:?}")),
    };
    match peripheral.start() {
        Ok(()) => log(step, "gatt_advertising", "status=Started"),
        Err(e) => {
            let (data, alias) = peripheral.last_advertisement_status();
            log(
                step,
                "gatt_advertising",
                &format!(
                    "error={e:?} refusal={:?} last_status_data={data} last_status_alias={alias}",
                    e.refusal()
                ),
            );
            // Step 3 still tries its publisher set on its own.
            if !publisher {
                return;
            }
        }
    }
    let mut set = None;
    if publisher {
        set = start_publisher(step, &raw);
    }
    let end = Instant::now() + Duration::from_secs(60);
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Some(p) = set {
        let _ = p.Stop();
        log(step, "publisher", "stopped");
    }
    peripheral.close();
    drop(peripheral);
    log(step, "end", "advertising stopped");
}

fn start_publisher(step: u8, raw: &[u8]) -> Option<BluetoothLEAdvertisementPublisher> {
    for with_uuid in [true, false] {
        let attempt = || -> windows::core::Result<BluetoothLEAdvertisementPublisher> {
            let publisher = BluetoothLEAdvertisementPublisher::new()?;
            let ad = publisher.Advertisement()?;
            let writer = DataWriter::new()?;
            writer.WriteBytes(raw)?;
            let data = BluetoothLEManufacturerData::new()?;
            data.SetCompanyId(0xffff)?;
            data.SetData(&writer.DetachBuffer()?)?;
            ad.ManufacturerData()?.Append(&data)?;
            if with_uuid {
                ad.ServiceUuids()?
                    .Append(GUID::from_u128(u128::from_be_bytes(CONTROL_SERVICE)))?;
            }
            publisher.StatusChanged(&TypedEventHandler::<
                BluetoothLEAdvertisementPublisher,
                BluetoothLEAdvertisementPublisherStatusChangedEventArgs,
            >::new(move |_, args| {
                let args = args.ok()?;
                println!(
                    "t={} step={step} event=publisher_status status={} error={}",
                    now_ms(),
                    args.Status()?.0,
                    args.Error()?.0
                );
                Ok(())
            }))?;
            publisher.Start()?;
            Ok(publisher)
        };
        match attempt() {
            Ok(p) => {
                log(
                    step,
                    "publisher",
                    &format!(
                        "started connectable=false service_uuid={with_uuid} manufacturer=ffff:{}",
                        hex(raw)
                    ),
                );
                return Some(p);
            }
            // The UUID-free variant is the second, logged variant of this one step.
            Err(e) => log(
                step,
                "publisher",
                &format!("service_uuid={with_uuid} error={:#010x}", e.code().0),
            ),
        }
    }
    None
}

fn main() {
    if !std::env::args().any(|a| a == "--approved") {
        eprintln!(
            "radio_check transmits and scans on the real radio. Run only with --approved on test devices you own."
        );
        std::process::exit(2);
    }
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let caps = probe();
    let public = (|| {
        BluetoothAdapter::GetDefaultAsync()?
            .join()?
            .BluetoothAddress()
    })()
    .map(|a| format!("{a:012x}"))
    .unwrap_or_else(|e| format!("unavailable({:#010x})", e.code().0));
    log(
        0,
        "probe",
        &format!(
            "status=\"{}\" le={:?} central={:?} peripheral={:?} extended={:?} max_adv={:?} adapter_public_address={public} advertising_addresses=not_exposed_by_windows",
            caps.status(),
            caps.low_energy,
            caps.central,
            caps.peripheral,
            caps.extended_advertising,
            caps.max_advertisement_bytes
        ),
    );
    step1();
    std::thread::sleep(Duration::from_secs(3));
    let offer = test_offer();
    advertise(2, &offer, false);
    std::thread::sleep(Duration::from_secs(3));
    advertise(3, &offer, true);
    log(0, "done", "no connection initiated; all sets stopped");
}
