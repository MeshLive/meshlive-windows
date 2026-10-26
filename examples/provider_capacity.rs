//! Design QV W1 follow-up (run only with `--approved`): which combinations of
//! GattServiceProviders this adapter keeps on air. Advertising only, on this
//! PC; no scanning, no connection, no pairing, radio state never changed;
//! every RDI is random test data, not a MeshLive identity.
//!
//! W1 (2026-10-21) saw the 23-byte kind 1/2 provider report Aborted while
//! the compact call-offer alias was up, and the alias then aborted too. Each
//! case below starts the data service (c8d5a105, connectable, no data: stays
//! Created, as in the app) plus the case's announcement providers, samples
//! every provider's AdvertisementStatus every 10 ms for `--seconds` (12) and
//! prints each change with ms since the case started, then a summary line:
//! `case=... <provider>=<final status> aborted_ms=<total ms Aborted>`.
//!
//! cargo example: provider_capacity --approved [--seconds 12] [--only <case>]
use meshlive::discovery::{ACCEPTING, Announcement, GATT};
use std::time::{Duration, Instant};
use windows::{
    Devices::Bluetooth::{BluetoothAdapter, BluetoothError, GenericAttributeProfile::*},
    Storage::Streams::DataWriter,
    Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize},
    core::GUID,
};

const DATA: u128 = 0xc8d5a105_9f88_4f65_a03d_7f5a2fb921fa;
const PC: u128 = meshlive::pc_announcement::UUID;
const WRITE: GUID = GUID::from_u128(0xc8d5a103_9f88_4f65_a03d_7f5a2fb921fa);
const NOTIFY: GUID = GUID::from_u128(0xc8d5a104_9f88_4f65_a03d_7f5a2fb921fa);

fn status_name(status: i32) -> &'static str {
    match status {
        0 => "Created",
        1 => "Stopped",
        2 => "Started",
        3 => "Aborted",
        4 => "StartedWithoutAll",
        _ => "None",
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn random<const N: usize>() -> [u8; N] {
    meshlive::crypto::random_nonce().expect("system RNG")[..N].try_into().unwrap()
}
fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

/// One announcement provider of a case.
#[derive(Clone)]
struct Spec {
    label: &'static str,
    uuid: u128,
    data: Vec<u8>,
    /// LE 1M secondary PHY (the app's c8d5a10a form); false: the app's compact alias form.
    extended: bool,
}

fn kind0() -> [u8; 9] {
    meshlive::pc_announcement::CallOffer { rdi: random(), relay: false, consent: false, codecs: 3, cost: 0 }.encode().unwrap()
}
fn kind1() -> [u8; 23] {
    let rdi: [u8; 6] = random();
    let request = Announcement {
        version: 5,
        prefix: rdi,
        flags: ACCEPTING | GATT,
        summary: 0,
        token: meshlive::bluetooth::advertise::route_token(0x0051_7e57, &rdi).unwrap(),
        cost: 0,
        sequence: 0,
        width: 0,
        height: 0,
        fps: 0,
        bitrate: 500,
        recovery: None,
    };
    meshlive::pc_announcement::wrap(meshlive::pc_announcement::KIND_VIEWER_REQUEST, &request).unwrap()
}
fn alias() -> Spec {
    let uuid = meshlive::bluetooth::advertise::pc_compact_alias(&kind0()).unwrap();
    Spec { label: "alias", uuid: u128::from_be_bytes(uuid), data: vec![0], extended: false }
}
fn pc(label: &'static str, data: Vec<u8>) -> Spec {
    Spec { label, uuid: PC, data, extended: true }
}

fn cases() -> Vec<(&'static str, Vec<Spec>)> {
    let other = u128::from_be_bytes(random()) & !(0xffff_u128 << 112) | (0x5e57_u128 << 112);
    vec![
        ("alias_only", vec![alias()]),
        ("k0_only", vec![pc("k0_9", kind0().to_vec())]),
        ("k1_23_only", vec![pc("k1_23", kind1().to_vec())]),
        ("k1_9_only", vec![pc("k1_9", kind1()[..9].to_vec())]),
        ("k1_16_only", vec![pc("k1_16", kind1()[..16].to_vec())]),
        ("k1_20_only", vec![pc("k1_20", kind1()[..20].to_vec())]),
        ("alias+k0", vec![alias(), pc("k0_9", kind0().to_vec())]),
        ("alias+k1_23", vec![alias(), pc("k1_23", kind1().to_vec())]),
        ("alias+k1_23_other_uuid", vec![alias(), Spec { label: "k1_23_other", uuid: other, data: kind1().to_vec(), extended: true }]),
        ("alias+k1_9", vec![alias(), pc("k1_9", kind1()[..9].to_vec())]),
        ("alias+k1_23_legacy", vec![alias(), Spec { label: "k1_23_legacy", uuid: PC, data: kind1().to_vec(), extended: false }]),
        ("alias+k1_23_drop", vec![alias(), pc("k1_23", kind1().to_vec())]),
        ("alias+k1_23_restart", vec![alias(), pc("k1_23", kind1().to_vec())]),
        ("k0+k1_23", vec![pc("k0_9", kind0().to_vec()), Spec { label: "k1_23_other", uuid: other, data: kind1().to_vec(), extended: true }]),
    ]
}

struct Live {
    label: &'static str,
    provider: GattServiceProvider,
}
impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.provider.StopAdvertising();
    }
}

fn create(uuid: u128, characteristics: bool) -> windows::core::Result<GattServiceProvider> {
    let result = GattServiceProvider::CreateAsync(GUID::from_u128(uuid))?.join()?;
    if result.Error()? != BluetoothError::Success {
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(-1)));
    }
    let provider = result.ServiceProvider()?;
    if characteristics {
        let rx = GattLocalCharacteristicParameters::new()?;
        rx.SetCharacteristicProperties(GattCharacteristicProperties::Write | GattCharacteristicProperties::WriteWithoutResponse)?;
        rx.SetWriteProtectionLevel(GattProtectionLevel::Plain)?;
        let tx = GattLocalCharacteristicParameters::new()?;
        tx.SetCharacteristicProperties(GattCharacteristicProperties::Notify)?;
        tx.SetReadProtectionLevel(GattProtectionLevel::Plain)?;
        provider.Service()?.CreateCharacteristicAsync(WRITE, &rx)?.join()?;
        provider.Service()?.CreateCharacteristicAsync(NOTIFY, &tx)?.join()?;
    }
    Ok(provider)
}

fn start(spec: &Spec) -> windows::core::Result<Live> {
    // The app's c8d5a10a provider carries the write/notify pair; the alias none.
    let provider = create(spec.uuid, spec.extended)?;
    let params = GattServiceProviderAdvertisingParameters::new()?;
    params.SetIsDiscoverable(false)?;
    params.SetIsConnectable(true)?;
    if spec.extended {
        params.SetUseLowEnergyUncoded1MPhyAsSecondaryPhy(true)?;
    }
    let writer = DataWriter::new()?;
    writer.WriteBytes(&spec.data)?;
    params.SetServiceData(&writer.DetachBuffer()?)?;
    provider.StartAdvertisingWithParameters(&params)?;
    Ok(Live { label: spec.label, provider })
}

fn run(case: &str, specs: &[Spec], seconds: u64) {
    let begin = Instant::now();
    let ms = || begin.elapsed().as_millis();
    let detail: Vec<String> = specs.iter().map(|s| format!("{}:uuid={:032x}:len={}:ext={}:data={}", s.label, s.uuid, s.data.len(), s.extended, hex(&s.data))).collect();
    println!("case={case} begin {}", detail.join(" "));
    let data = match create(DATA, true).and_then(|p| {
        let params = GattServiceProviderAdvertisingParameters::new()?;
        params.SetIsDiscoverable(false)?;
        params.SetIsConnectable(true)?;
        p.StartAdvertisingWithParameters(&params)?;
        Ok(Live { label: "data", provider: p })
    }) {
        Ok(live) => live,
        Err(e) => {
            println!("case={case} data_failed {e:?}");
            return;
        }
    };
    let mut live = vec![data];
    for spec in specs {
        std::thread::sleep(Duration::from_millis(200));
        match start(spec) {
            Ok(l) => live.push(l),
            Err(e) => println!("case={case} t={} {} start_failed {e:?}", ms(), spec.label),
        }
    }
    let mut last: Vec<i32> = vec![-2; live.len()];
    let mut aborted = vec![0u128; live.len()];
    let until = begin + Duration::from_secs(seconds);
    // _drop: the second provider stops at 3 s (the app's settle gives up);
    // _restart: it stops at 3 s and a new one starts (the app's periodic restart).
    let mut dropped = false;
    while Instant::now() < until {
        if !dropped && (case.ends_with("_drop") || case.ends_with("_restart")) && begin.elapsed() >= Duration::from_secs(3) && live.len() > 2 {
            dropped = true;
            let gone = live.pop().unwrap();
            println!("case={case} t={} {} stop", ms(), gone.label);
            drop(gone);
            last.pop();
            aborted.pop();
            if case.ends_with("_restart") {
                match start(&specs[specs.len() - 1]) {
                    Ok(l) => {
                        println!("case={case} t={} {} restart", ms(), l.label);
                        live.push(l);
                        last.push(-2);
                        aborted.push(0);
                    }
                    Err(e) => println!("case={case} t={} restart_failed {e:?}", ms()),
                }
            }
        }
        for (i, l) in live.iter().enumerate() {
            let s = l.provider.AdvertisementStatus().map_or(-1, |s| s.0);
            if s == 3 {
                aborted[i] += 10;
            }
            if s != last[i] {
                println!("case={case} t={} {} status={}({})", ms(), l.label, s, status_name(s));
                last[i] = s;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let summary: Vec<String> = live.iter().enumerate().map(|(i, l)| format!("{}={} aborted_ms={}", l.label, status_name(last[i]), aborted[i])).collect();
    println!("case={case} summary {}", summary.join(" "));
    drop(live);
    std::thread::sleep(Duration::from_secs(2));
}

fn main() {
    if !std::env::args().any(|a| a == "--approved") {
        eprintln!("Advertises on this PC's radio; run only with --approved.");
        std::process::exit(2);
    }
    // SAFETY: once, before any WinRT call on this thread.
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let seconds = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(12);
    let only = arg("--only");
    match BluetoothAdapter::GetDefaultAsync().and_then(|a| a.join()) {
        Ok(adapter) => println!(
            "adapter extended_advertising={:?} max_advertisement_data_length={:?} peripheral_role={:?} central_role={:?} le_secure={:?}",
            adapter.IsExtendedAdvertisingSupported().ok(),
            adapter.MaxAdvertisementDataLength().ok(),
            adapter.IsPeripheralRoleSupported().ok(),
            adapter.IsCentralRoleSupported().ok(),
            adapter.AreLowEnergySecureConnectionsSupported().ok(),
        ),
        Err(e) => println!("adapter unavailable {e:?}"),
    }
    for (case, specs) in cases() {
        if only.as_deref().is_some_and(|o| o != case) {
            continue;
        }
        run(case, &specs, seconds);
    }
}
