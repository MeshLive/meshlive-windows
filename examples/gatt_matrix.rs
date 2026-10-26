//! Root-approved GATT advertising diagnosis (run only with `--approved`; about
//! 85 s). No connection is initiated and nothing is paired; radio state is not
//! changed. Each case creates one fresh GattServiceProvider with the MeshLive
//! write/notify characteristics, registers AdvertisementStatusChanged BEFORE
//! starting, starts advertising and logs every status for up to 20 s, then
//! stops and releases it. Case "discoverable" advertises this PC's friendly
//! name; it runs only with `--allow-discoverable`. `--qw-sweep` runs only the
//! Design QW hard gate 1 sweep (PC announcement UUID with 0/1/5/9 bytes);
//! `--qw-matrix` the second round (data provider, connectable, size, legacy
//! vs extended advertising). `--qw-hold MINUTES [--restart-at SECONDS]
//! [--non-connectable]` (connectable by default: Windows aborts a
//! non-connectable provider) is Design QW hard gate 2 (the address timeline): ONE
//! extended (LE 1M secondary) announcement with 9 bytes held for MINUTES with
//! no restart; with `--restart-at` it is stopped and started again once at
//! that second (a simulated epoch boundary; the payload's first byte changes
//! 0x5a -> 0xa5 so observers can tell the two epochs apart). Every start/stop
//! is logged with UTC ms, plus a status heartbeat every 60 s.
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::{
    Devices::Bluetooth::{BluetoothError, GenericAttributeProfile::*},
    Foundation::TypedEventHandler,
    Storage::Streams::DataWriter,
    Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize},
    core::GUID,
};

const DATA: GUID = GUID::from_u128(0xc8d5a105_9f88_4f65_a03d_7f5a2fb921fa);
const CONTROL: GUID = GUID::from_u128(0xc8d5a101_9f88_4f65_a03d_7f5a2fb921fa);
const WRITE: GUID = GUID::from_u128(0xc8d5a103_9f88_4f65_a03d_7f5a2fb921fa);
const NOTIFY: GUID = GUID::from_u128(0xc8d5a104_9f88_4f65_a03d_7f5a2fb921fa);
/// Design QW (A2): the fixed PC announcement service-data UUID.
const PC_ANNOUNCEMENT: u128 = 0xc8d5a10a_9f88_4f65_a03d_7f5a2fb921fa;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
/// GattServiceProviderAdvertisementStatus as a name (logs carry both).
fn status_name(status: i32) -> String {
    let name = match status {
        0 => "Created",
        1 => "Stopped",
        2 => "Started",
        3 => "Aborted",
        4 => "StartedWithoutAllAdvertisementData",
        _ => "Unknown",
    };
    format!("{status}({name})")
}
fn log(case: &str, event: &str, detail: &str) {
    println!("t={} case={case} event={event} {detail}", now_ms());
}

struct Case {
    name: &'static str,
    service: GUID,
    /// None = the parameterless StartAdvertising().
    params: Option<(bool, bool)>,
    service_data: Option<Vec<u8>>,
    /// Windows 11 24H2+: ask for extended advertising with LE 1M secondary PHY.
    secondary_1m: bool,
    /// How long the status is watched.
    watch_s: u64,
}

fn value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

/// Design QW gate 2: one announcement held without restarts (see the header).
fn qw_hold(minutes: u64, restart_at: Option<u64>, connectable: bool) {
    let total = minutes * 60;
    let restart_at = restart_at.filter(|&r| r > 0 && r < total);
    log("qw_hold", "plan", &format!("minutes={minutes} restart_at_s={restart_at:?} connectable={connectable} extended=1m bytes=9 utc_ms_start={}", now_ms()));
    let phases: Vec<(&'static str, u8, u64)> = match restart_at {
        Some(r) => vec![("qw_hold_epoch1", 0x5a, r), ("qw_hold_epoch2", 0xa5, total - r)],
        None => vec![("qw_hold", 0x5a, total)],
    };
    for (name, first, seconds) in phases {
        let payload: Vec<u8> = (0..9u8).map(|i| first ^ i).collect();
        let hex: String = payload.iter().map(|b| format!("{b:02x}")).collect();
        log(name, "payload", &format!("service_data={hex} hold_s={seconds}"));
        let case = Case {
            name,
            service: GUID::from_u128(PC_ANNOUNCEMENT),
            params: Some((connectable, false)),
            service_data: Some(payload),
            secondary_1m: true,
            watch_s: seconds,
        };
        match run(&case) {
            Ok(false) => {}
            Ok(true) => {
                log("qw_hold", "failed", "the announcement provider stuck at Aborted; the run is invalid for the PC side");
                std::process::exit(3);
            }
            Err(e) => {
                log(name, "error", &format!("hresult={:#010x}", e.code().0));
                std::process::exit(3);
            }
        }
    }
    log("qw_hold", "done", &format!("utc_ms_end={} no connection initiated", now_ms()));
}

/// Ok(true) when a long hold was abandoned because the status stuck at
/// Aborted (nothing on air; a heartbeat would only pretend it runs).
fn run(case: &Case) -> windows::core::Result<bool> {
    log(case.name, "create", "");
    let result = GattServiceProvider::CreateAsync(case.service)?.join()?;
    if result.Error()? != BluetoothError::Success {
        log(case.name, "error", &format!("create={}", result.Error()?.0));
        return Ok(false);
    }
    let provider = result.ServiceProvider()?;
    let service = provider.Service()?;
    let rx = GattLocalCharacteristicParameters::new()?;
    rx.SetCharacteristicProperties(
        GattCharacteristicProperties::Write | GattCharacteristicProperties::WriteWithoutResponse,
    )?;
    rx.SetWriteProtectionLevel(GattProtectionLevel::Plain)?;
    let tx = GattLocalCharacteristicParameters::new()?;
    tx.SetCharacteristicProperties(GattCharacteristicProperties::Notify)?;
    tx.SetReadProtectionLevel(GattProtectionLevel::Plain)?;
    let a = service.CreateCharacteristicAsync(WRITE, &rx)?.join()?;
    let b = service.CreateCharacteristicAsync(NOTIFY, &tx)?.join()?;
    log(
        case.name,
        "characteristics",
        &format!("write={} notify={}", a.Error()?.0, b.Error()?.0),
    );
    // A write handler is registered so a stray request is answered, never left pending.
    let write = a.Characteristic()?;
    let write_token = write.WriteRequested(&TypedEventHandler::<
        GattLocalCharacteristic,
        GattWriteRequestedEventArgs,
    >::new(|_, args| {
        if let Some(args) = args.as_ref() {
            let deferral = args.GetDeferral()?;
            deferral.Complete()?;
        }
        Ok(())
    }))?;
    let name = case.name;
    let token = provider.AdvertisementStatusChanged(&TypedEventHandler::<
        GattServiceProvider,
        GattServiceProviderAdvertisementStatusChangedEventArgs,
    >::new(move |_, args| {
        let args = args.ok()?;
        log(
            name,
            "status_event",
            &format!("status={} error={}", status_name(args.Status()?.0), args.Error()?.0),
        );
        Ok(())
    }))?;
    let started = Instant::now();
    match case.params {
        None => provider.StartAdvertising()?,
        Some((connectable, discoverable)) => {
            let p = GattServiceProviderAdvertisingParameters::new()?;
            p.SetIsConnectable(connectable)?;
            p.SetIsDiscoverable(discoverable)?;
            if let Some(data) = &case.service_data {
                let w = DataWriter::new()?;
                w.WriteBytes(data)?;
                if case.secondary_1m {
                    if let Err(e) = p.SetUseLowEnergyUncoded1MPhyAsSecondaryPhy(true) {
                        log(case.name, "secondary_phy_refused", &format!("hresult={:#010x}", e.code().0));
                    }
                }
                if let Err(e) = p.SetServiceData(&w.DetachBuffer()?) {
                    log(case.name, "service_data_refused", &format!("bytes={} hresult={:#010x}", data.len(), e.code().0));
                    return Ok(false);
                }
            }
            provider.StartAdvertisingWithParameters(&p)?;
        }
    }
    log(case.name, "start_called", "");
    let mut last = -1;
    let mut heartbeat = Instant::now();
    let mut aborted_since: Option<Instant> = None;
    let mut abandoned = false;
    while started.elapsed() < Duration::from_secs(case.watch_s) {
        let status = provider.AdvertisementStatus()?.0;
        if status == 3 {
            let since = *aborted_since.get_or_insert_with(Instant::now);
            // Long holds only (the matrix's short cases record the abort as data).
            if case.watch_s > 30 && since.elapsed() >= Duration::from_secs(10) {
                log(case.name, "aborted", &format!("status={} for 10 s: nothing is on air; hold abandoned", status_name(status)));
                abandoned = true;
                break;
            }
        } else {
            aborted_since = None;
        }
        if heartbeat.elapsed() >= Duration::from_secs(60) {
            heartbeat = Instant::now();
            log(case.name, "heartbeat", &format!("status={} after_s={}", status_name(status), started.elapsed().as_secs()));
        }
        if status != last {
            log(
                case.name,
                "status_poll",
                &format!("status={} after_ms={}", status_name(status), started.elapsed().as_millis()),
            );
            last = status;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    provider.StopAdvertising()?;
    let _ = provider.RemoveAdvertisementStatusChanged(token);
    let _ = write.RemoveWriteRequested(write_token);
    log(
        case.name,
        "stopped",
        &format!("status={}", status_name(provider.AdvertisementStatus()?.0)),
    );
    Ok(abandoned)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--approved") {
        eprintln!(
            "gatt_matrix advertises on the real radio. Run only with --approved on test devices you own."
        );
        std::process::exit(2);
    }
    let discoverable = args.iter().any(|a| a == "--allow-discoverable");
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    if args.iter().any(|a| a == "--qw-sweep") {
        qw_sweep();
        return;
    }
    if let Some(minutes) = value(&args, "--qw-hold").and_then(|v| v.parse::<u64>().ok()) {
        let restart_at = value(&args, "--restart-at").and_then(|v| v.parse::<u64>().ok());
        // Connectable by default: Windows aborts a non-connectable provider
        // (gate 1 matrix and gate 2 run 1: every conn0 case stayed Aborted).
        qw_hold(minutes, restart_at, !args.iter().any(|a| a == "--non-connectable"));
        return;
    }
    if args.iter().any(|a| a == "--qw-matrix") {
        qw_matrix();
        return;
    }
    let mut cases = vec![
        Case {
            name: "connectable_only",
            service: DATA,
            params: Some((true, false)),
            service_data: None,
            secondary_1m: false,
            watch_s: 20,
        },
        Case {
            name: "control_uuid_service_data",
            service: CONTROL,
            params: Some((true, false)),
            // A fixed, labelled test payload (22 bytes), not an identity.
            service_data: Some(vec![
                0x02, 0xfe, 0xed, 0, 0, 0, 1, 0x49, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ]),
            secondary_1m: false,
            watch_s: 20,
        },
    ];
    if args.iter().any(|a| a == "--only-compact-service-data") {
        // Approved case 3b: service data keyed by an iPhone-style compact call-offer
        // UUID built from a random test prefix, one zero byte of payload.
        let random = meshlive::crypto::random_nonce().expect("CNG random");
        let offer = meshlive::discovery::Announcement {
            version: 2,
            prefix: random[..6].try_into().unwrap(),
            flags: meshlive::discovery::ACCEPTING
                | meshlive::discovery::CALL
                | meshlive::discovery::GATT,
            summary: 0x01,
            token: 0,
            cost: 0,
            sequence: 0,
            width: 0,
            height: 0,
            fps: 0,
            bitrate: 0,
            recovery: None,
        };
        let uuid = offer.compact().expect("compact form");
        let payload = vec![0u8];
        // 128-bit Service Data AD structure: length, type 0x21, UUID little-endian, data.
        let mut ad = vec![(1 + 16 + payload.len()) as u8, 0x21];
        ad.extend(uuid.iter().rev());
        ad.extend(&payload);
        let hex: String = ad.iter().map(|b| format!("{b:02x}")).collect();
        log(
            "compact_service_data",
            "expected_ad",
            &format!(
                "test_prefix={} uuid={} service_data_ad={hex} ad_bytes={} plus_flags_if_any=3 (computed; Windows does not report its PDU)",
                offer
                    .prefix
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
                uuid.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                ad.len()
            ),
        );
        cases = vec![Case {
            name: "compact_service_data",
            service: GUID::from_u128(u128::from_be_bytes(uuid)),
            params: Some((true, false)),
            service_data: Some(payload),
            secondary_1m: false,
            watch_s: 20,
        }];
    }
    if discoverable {
        cases.push(Case {
            name: "discoverable",
            service: DATA,
            params: Some((true, true)),
            service_data: None,
            secondary_1m: false,
            watch_s: 20,
        });
    }
    log(
        "all",
        "begin",
        &format!("cases={} allow_discoverable={discoverable}", cases.len()),
    );
    for case in &cases {
        if let Err(e) = run(case) {
            log(case.name, "error", &format!("hresult={:#010x}", e.code().0));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    log("all", "done", "no connection initiated");
}

/// Design QW hard gate 1: the announcement provider with 0, 1, 5 and 9
/// service-data bytes, next to a running c8d5a105 data provider as in the
/// product. Every status is logged for 20 s (also past Aborted), with the
/// AD bytes an observer must see. The payload is a labelled test pattern,
/// not an identity.
fn qw_sweep() {
    let data = match GattServiceProvider::CreateAsync(DATA).and_then(|op| op.join()) {
        Ok(result) => result.ServiceProvider().ok(),
        Err(_) => None,
    };
    if let Some(provider) = &data {
        let p = GattServiceProviderAdvertisingParameters::new().unwrap();
        let _ = p.SetIsConnectable(true);
        let _ = p.SetIsDiscoverable(false);
        let started = provider.StartAdvertisingWithParameters(&p);
        log("qw_data_provider", "start_called", &format!("ok={} status={}", started.is_ok(), status_name(provider.AdvertisementStatus().map_or(-1, |s| s.0))));
    }
    for size in [0usize, 1, 5, 9] {
        // 0x5A..: a labelled test pattern, one byte per position.
        let payload: Vec<u8> = (0..size as u8).map(|i| 0x5a ^ i).collect();
        let mut ad = vec![(1 + 16 + payload.len()) as u8, 0x21];
        ad.extend(PC_ANNOUNCEMENT.to_be_bytes().iter().rev());
        ad.extend(&payload);
        let hex: String = ad.iter().map(|b| format!("{b:02x}")).collect();
        let name: &'static str = Box::leak(format!("qw_size_{size}").into_boxed_str());
        log(name, "expected_ad", &format!("service_data_ad={hex} ad_bytes={} with_flags={} legacy_limit=31", ad.len(), ad.len() + 3));
        let case = Case {
            name,
            service: GUID::from_u128(PC_ANNOUNCEMENT),
            params: Some((true, false)),
            service_data: Some(payload),
            secondary_1m: false,
            watch_s: 20,
        };
        if let Err(e) = run(&case) {
            log(name, "error", &format!("hresult={:#010x}", e.code().0));
        }
        if let Some(provider) = &data {
            log("qw_data_provider", "status", &format!("status={}", status_name(provider.AdvertisementStatus().map_or(-1, |s| s.0))));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    if let Some(provider) = &data {
        let _ = provider.StopAdvertising();
    }
    log("qw", "done", "no connection initiated");
}

/// Design QW gate 1, second round: which setting lets Windows include every
/// requested byte (status 2 Started instead of 4)? data provider off/on x
/// connectable true/false x 1/9 data bytes x legacy / extended (LE 1M
/// secondary PHY), 8 s each.
fn qw_matrix() {
    for data_on in [false, true] {
        let data = if data_on {
            GattServiceProvider::CreateAsync(DATA).and_then(|op| op.join()).ok().and_then(|r| r.ServiceProvider().ok())
        } else {
            None
        };
        if let Some(provider) = &data {
            let p = GattServiceProviderAdvertisingParameters::new().unwrap();
            let _ = p.SetIsConnectable(true);
            let _ = p.SetIsDiscoverable(false);
            let _ = provider.StartAdvertisingWithParameters(&p);
        }
        for connectable in [true, false] {
            for size in [1usize, 9] {
                for extended in [false, true] {
                    let payload: Vec<u8> = (0..size as u8).map(|i| 0x5a ^ i).collect();
                    let name: &'static str = Box::leak(
                        format!("qw_data{}_conn{}_size{size}_{}", u8::from(data_on), u8::from(connectable), if extended { "ext1m" } else { "legacy" })
                            .into_boxed_str(),
                    );
                    let case = Case {
                        name,
                        service: GUID::from_u128(PC_ANNOUNCEMENT),
                        params: Some((connectable, false)),
                        service_data: Some(payload),
                        secondary_1m: extended,
                        watch_s: 8,
                    };
                    if let Err(e) = run(&case) {
                        log(name, "error", &format!("hresult={:#010x}", e.code().0));
                    }
                    if let Some(provider) = &data {
                        log("qw_data_provider", "status", &format!("status={}", status_name(provider.AdvertisementStatus().map_or(-1, |s| s.0))));
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
        if let Some(provider) = &data {
            let _ = provider.StopAdvertising();
        }
    }
    log("qw_matrix", "done", "no connection initiated");
}
