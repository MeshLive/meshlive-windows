//! Design QV gate V1 (run only with `--approved`, on test devices you own; it advertises on the real radio, never connects, pairs or
//! changes radio state). Question: can a phone reach this PC's `c8d5a105`
//! data service through the advert of a provider whose service UUID is an
//! iPhone-style compact VIEWER REQUEST (version 2 rotating, kind 1)?
//!
//! It publishes two providers, both connectable and not discoverable:
//! 1. the data service `c8d5a105` (write `c8d5a103`, notify `c8d5a104`);
//! 2. the request provider: service UUID = the compact kind-1 UUID built from
//!    a random test RDI and route token, 1 byte of service data (Windows
//!    never starts a provider without data), extended with LE 1M secondary
//!    PHY (Windows then also sends a legacy UUID-list advert: QW gate 2).
//!
//! Everything is logged with UTC ms: the exact UUID and its on-air (LSB
//! first) bytes, status events of both providers, every subscription,
//! unsubscription and write on BOTH services with the peer's device tag and
//! max PDU, and a status heartbeat every 60 s. With `--restart-at S` the
//! request provider restarts once with a new RDI and token (a simulated epoch
//! boundary), the data service staying up.
//!
//! Pass criteria are judged by the run owner from this log plus the raw
//! observers (see docs/dev/design/watch-and-go-live.md, gate V1): exact UUID bytes on air in the
//! legacy UUID list, connectable PDU, a phone connection that lists and
//! subscribes to c8d5a105 (logged here as `subscribed service=c8d5a105`), and
//! a new address at the restart.
//!
//! cargo example: qv_gate --approved [--minutes 10] [--restart-at 300]
use meshlive::discovery::{ACCEPTING, Announcement, GATT};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::{
    Devices::Bluetooth::{BluetoothError, GenericAttributeProfile::*},
    Foundation::TypedEventHandler,
    Storage::Streams::DataWriter,
    Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize},
    core::{GUID, IInspectable},
};

const DATA: u128 = 0xc8d5a105_9f88_4f65_a03d_7f5a2fb921fa;
const WRITE: GUID = GUID::from_u128(0xc8d5a103_9f88_4f65_a03d_7f5a2fb921fa);
const NOTIFY: GUID = GUID::from_u128(0xc8d5a104_9f88_4f65_a03d_7f5a2fb921fa);

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}
fn log(scope: &str, event: &str, detail: &str) {
    println!("t={} scope={scope} event={event} {detail}", now_ms());
}
fn status_name(status: i32) -> &'static str {
    match status {
        0 => "Created",
        1 => "Stopped",
        2 => "Started",
        3 => "Aborted",
        4 => "StartedWithoutAllAdvertisementData",
        _ => "Unknown",
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn arg(name: &str) -> Option<u64> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)?.parse().ok())
}
/// A short, non-identifying tag of a GATT session's device ID (the address part).
fn peer_tag(id: &str) -> String {
    let tail: String = id.chars().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect();
    format!("..{tail}")
}
fn random_bytes<const N: usize>() -> [u8; N] {
    let nonce = meshlive::crypto::random_nonce().expect("system RNG");
    nonce[..N].try_into().unwrap()
}

/// The compact kind-1 viewer request for a random test RDI and token.
fn request_uuid() -> (Announcement, [u8; 16]) {
    let token = u32::from_be_bytes(random_bytes::<4>()) | 0x8000_0000;
    let request = Announcement {
        version: 5,
        prefix: random_bytes::<6>(),
        flags: ACCEPTING | GATT,
        summary: 0,
        token,
        cost: 0,
        sequence: 0,
        width: 0,
        height: 0,
        fps: 0,
        bitrate: 300,
        recovery: None,
    };
    let uuid = request.compact().expect("compact viewer request");
    (request, uuid)
}

struct Published {
    provider: GattServiceProvider,
    name: &'static str,
}
impl Drop for Published {
    fn drop(&mut self) {
        let _ = self.provider.StopAdvertising();
        log(self.name, "stopped", "");
    }
}

/// One provider with the MeshLive write/notify characteristics; every write
/// and subscription change is logged (writes are acknowledged, never read).
fn publish(name: &'static str, service: GUID, data: Option<&[u8]>, secondary_1m: bool) -> windows::core::Result<Published> {
    let result = GattServiceProvider::CreateAsync(service)?.join()?;
    if result.Error()? != BluetoothError::Success {
        log(name, "create_failed", &format!("error={}", result.Error()?.0));
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(-1)));
    }
    let provider = result.ServiceProvider()?;
    let local = provider.Service()?;
    let rx = GattLocalCharacteristicParameters::new()?;
    rx.SetCharacteristicProperties(GattCharacteristicProperties::Write | GattCharacteristicProperties::WriteWithoutResponse)?;
    rx.SetWriteProtectionLevel(GattProtectionLevel::Plain)?;
    let tx = GattLocalCharacteristicParameters::new()?;
    tx.SetCharacteristicProperties(GattCharacteristicProperties::Notify)?;
    tx.SetReadProtectionLevel(GattProtectionLevel::Plain)?;
    let write = local.CreateCharacteristicAsync(WRITE, &rx)?.join()?.Characteristic()?;
    let notify = local.CreateCharacteristicAsync(NOTIFY, &tx)?.join()?.Characteristic()?;
    write.WriteRequested(&TypedEventHandler::<GattLocalCharacteristic, GattWriteRequestedEventArgs>::new(move |_, args| {
        if let Some(args) = args.as_ref() {
            let deferral = args.GetDeferral()?;
            let peer = args.Session().and_then(|s| s.DeviceId()?.Id()).map(|id| peer_tag(&id.to_string())).unwrap_or_default();
            let bytes = args.GetRequestAsync().and_then(|r| r.join()).and_then(|r| r.Value()).and_then(|v| v.Length()).unwrap_or(0);
            log(name, "write", &format!("char=c8d5a103 bytes={bytes} peer={peer}"));
            deferral.Complete()?;
        }
        Ok(())
    }))?;
    notify.SubscribedClientsChanged(&TypedEventHandler::<GattLocalCharacteristic, IInspectable>::new(move |sender, _| {
        if let Some(sender) = sender.as_ref() {
            let clients = sender.SubscribedClients()?;
            let mut peers = Vec::new();
            for client in &clients {
                let session = client.Session()?;
                peers.push(format!("{}@{}", peer_tag(&session.DeviceId()?.Id()?.to_string()), session.MaxPduSize().unwrap_or(0)));
            }
            log(name, "subscribed", &format!("char=c8d5a104 clients={} peers={peers:?}", clients.Size()?));
        }
        Ok(())
    }))?;
    provider.AdvertisementStatusChanged(&TypedEventHandler::<GattServiceProvider, GattServiceProviderAdvertisementStatusChangedEventArgs>::new(move |_, args| {
        if let Some(args) = args.as_ref() {
            log(name, "status_event", &format!("status={} error={}", status_name(args.Status()?.0), args.Error()?.0));
        }
        Ok(())
    }))?;
    let p = GattServiceProviderAdvertisingParameters::new()?;
    p.SetIsConnectable(true)?;
    p.SetIsDiscoverable(false)?;
    if let Some(data) = data {
        if secondary_1m {
            if let Err(e) = p.SetUseLowEnergyUncoded1MPhyAsSecondaryPhy(true) {
                log(name, "secondary_phy_refused", &format!("hresult={:#010x}", e.code().0));
            }
        }
        let w = DataWriter::new()?;
        w.WriteBytes(data)?;
        p.SetServiceData(&w.DetachBuffer()?)?;
    }
    provider.StartAdvertisingWithParameters(&p)?;
    log(name, "start_called", &format!("service={service:?} data={}", data.map(hex).unwrap_or_else(|| "none".into())));
    Ok(Published { provider, name })
}

fn main() {
    if !std::env::args().any(|a| a == "--approved") {
        eprintln!("qv_gate advertises on the real radio. Run only with --approved on test devices you own.");
        std::process::exit(2);
    }
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let total = Duration::from_secs(arg("--minutes").unwrap_or(10).clamp(1, 30) * 60);
    let restart_at = arg("--restart-at").map(Duration::from_secs).filter(|r| *r < total);
    log("qv_gate", "plan", &format!("minutes={} restart_at_s={:?} utc_ms_start={}", total.as_secs() / 60, restart_at.map(|r| r.as_secs()), now_ms()));
    let data = match publish("data_c8d5a105", GUID::from_u128(DATA), None, false) {
        Ok(p) => p,
        Err(e) => {
            log("qv_gate", "failed", &format!("data service hresult={:#010x}", e.code().0));
            std::process::exit(3);
        }
    };
    let start = Instant::now();
    let mut phase = 1;
    let mut request: Option<Published> = None;
    let mut heartbeat = Instant::now();
    while start.elapsed() < total {
        let want_restart = phase == 1 && restart_at.is_some_and(|r| start.elapsed() >= r);
        if request.is_none() || want_restart {
            if want_restart {
                drop(request.take());
                phase = 2;
            }
            let (announcement, uuid) = request_uuid();
            let guid = GUID::from_u128(u128::from_be_bytes(uuid));
            let mut on_air = uuid;
            on_air.reverse();
            log(
                "request",
                "uuid",
                &format!(
                    "phase={phase} uuid={guid:?} on_air_lsb_first={} rdi={} token={:08x} bitrate_kbps={}",
                    hex(&on_air),
                    hex(&announcement.prefix),
                    announcement.token,
                    announcement.bitrate
                ),
            );
            match publish(if phase == 1 { "request_phase1" } else { "request_phase2" }, guid, Some(&[0x01]), true) {
                Ok(p) => request = Some(p),
                Err(e) => {
                    log("qv_gate", "failed", &format!("request provider hresult={:#010x}", e.code().0));
                    std::process::exit(3);
                }
            }
        }
        if heartbeat.elapsed() >= Duration::from_secs(60) {
            heartbeat = Instant::now();
            let status = |p: &Published| p.provider.AdvertisementStatus().map(|s| status_name(s.0)).unwrap_or("error");
            log("qv_gate", "heartbeat", &format!("after_s={} data={} request={}", start.elapsed().as_secs(), status(&data), request.as_ref().map(status).unwrap_or("none")));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    drop(request);
    drop(data);
    log("qv_gate", "done", &format!("utc_ms_end={} no connection initiated by this PC", now_ms()));
}
