//! Unfiltered BLE observer for the Design QW privacy gate (run only with
//! `--approved`; it scans with the real radio and never transmits in passive
//! mode). Meant for a SECOND PC next to the advertising one: Windows does not
//! report its own adverts to its own watcher.
//!
//! Every report is one line: UTC ms, address, address type (public/random as
//! Windows reports it), advertisement type, connectable/scannable/directed/
//! anonymous, RSSI and every AD section as `type:hex`. `--contains HEX` keeps
//! only reports whose AD bytes contain HEX (e.g. a UUID in on-air byte order).
//! At the end, one summary line per distinct payload lists the addresses it
//! came from (a payload seen from several addresses = a rotating address).
//!
//! Microsoft Q&A (2021-05-13, accepted answer): "Windows currently doesn't
//! use private random address when connecting to the device (it only
//! randomizes during advertising)". That is the claim this tool checks.
//!
//! cargo example: observe_adverts --approved [--seconds 60] [--active] [--contains HEX]
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    Devices::Bluetooth::Advertisement::{
        BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher, BluetoothLEScanningMode,
    },
    Foundation::TypedEventHandler,
    Storage::Streams::DataReader,
    Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize},
};

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}
fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One received report, decoded for the log (pure, testable).
pub struct Report {
    pub address: u64,
    pub address_type: i32,
    pub kind: i32,
    pub flags: [Option<bool>; 4],
    pub rssi: i16,
    pub sections: Vec<(u8, Vec<u8>)>,
}
impl Report {
    pub fn payload_key(&self) -> String {
        self.sections.iter().map(|(t, d)| format!("{t:02x}:{}", hex(d))).collect::<Vec<_>>().join(" ")
    }
    pub fn line(&self) -> String {
        let flag = |v: Option<bool>| v.map_or("?".to_owned(), |b| b.to_string());
        format!(
            "address={:012x} address_type={} (0 public, 1 random) adv_type={} connectable={} scannable={} directed={} anonymous={} rssi={} ad={}",
            self.address,
            self.address_type,
            self.kind,
            flag(self.flags[0]),
            flag(self.flags[1]),
            flag(self.flags[2]),
            flag(self.flags[3]),
            self.rssi,
            self.payload_key()
        )
    }
    pub fn contains(&self, needle: &[u8]) -> bool {
        needle.is_empty() || self.sections.iter().any(|(_, d)| d.windows(needle.len()).any(|w| w == needle))
    }
}

fn read(args: &BluetoothLEAdvertisementReceivedEventArgs) -> windows::core::Result<Report> {
    let ad = args.Advertisement()?;
    let mut sections = Vec::new();
    for section in ad.DataSections()? {
        let buffer = section.Data()?;
        let reader = DataReader::FromBuffer(&buffer)?;
        let mut bytes = vec![0u8; buffer.Length()? as usize];
        reader.ReadBytes(&mut bytes)?;
        sections.push((section.DataType()?, bytes));
    }
    Ok(Report {
        address: args.BluetoothAddress()?,
        address_type: args.BluetoothAddressType().map_or(-1, |t| t.0),
        kind: args.AdvertisementType()?.0,
        flags: [
            args.IsConnectable().ok(),
            args.IsScannable().ok(),
            args.IsDirected().ok(),
            args.IsAnonymous().ok(),
        ],
        rssi: args.RawSignalStrengthInDBm()?,
        sections,
    })
}

fn main() {
    if !std::env::args().any(|a| a == "--approved") {
        eprintln!("observe_adverts uses the real radio. Run only with --approved on test devices you own.");
        std::process::exit(2);
    }
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(60);
    let needle: Vec<u8> = arg("--contains")
        .map(|h| (0..h.len() / 2).filter_map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).ok()).collect())
        .unwrap_or_default();
    let active = std::env::args().any(|a| a == "--active");
    if let Err(error) = run(seconds, needle, active) {
        println!("t={} phase=failed {error:?}", now_ms());
        std::process::exit(1);
    }
}

fn run(seconds: u64, needle: Vec<u8>, active: bool) -> windows::core::Result<()> {
    unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
    let watcher = BluetoothLEAdvertisementWatcher::new()?;
    // Passive by default: the observer sends no scan requests.
    watcher.SetScanningMode(if active { BluetoothLEScanningMode::Active } else { BluetoothLEScanningMode::Passive })?;
    let seen: Arc<Mutex<BTreeMap<String, BTreeSet<(u64, i32)>>>> = Arc::default();
    let sink = seen.clone();
    let token = watcher.Received(&TypedEventHandler::<BluetoothLEAdvertisementWatcher, BluetoothLEAdvertisementReceivedEventArgs>::new(
        move |_, args| {
            if let Ok(report) = read(&args.ok()?.clone()) {
                if report.contains(&needle) {
                    println!("t={} phase=report {}", now_ms(), report.line());
                    if let Ok(mut map) = sink.lock() {
                        map.entry(report.payload_key()).or_default().insert((report.address, report.address_type));
                    }
                }
            }
            Ok(())
        },
    ))?;
    println!("t={} phase=start seconds={seconds} mode={}", now_ms(), if active { "active" } else { "passive" });
    watcher.Start()?;
    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(200));
    }
    watcher.Stop()?;
    let _ = watcher.RemoveReceived(token);
    for (payload, addresses) in seen.lock().unwrap().iter() {
        let list: Vec<String> = addresses.iter().map(|(a, t)| format!("{a:012x}/{t}")).collect();
        println!("t={} phase=summary addresses={} [{}] ad={payload}", now_ms(), addresses.len(), list.join(","));
    }
    println!("t={} phase=done nothing was transmitted in passive mode", now_ms());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_report_line_names_the_address_type_and_matches_on_air_bytes() {
        // Design QW A2: the PC announcement UUID c8d5a10a-... in on-air (LSB first) order.
        let uuid_on_air: Vec<u8> = (0..16).rev().map(|i| (0xc8d5a10a_9f88_4f65_a03d_7f5a2fb921fau128 >> (8 * i)) as u8).rev().collect();
        let report = Report {
            address: 0x4438e8239460,
            address_type: 1,
            kind: 0,
            flags: [Some(true), Some(false), Some(false), None],
            rssi: -40,
            sections: vec![(0x01, vec![0x06]), (0x21, [uuid_on_air.clone(), vec![0, 1, 2]].concat())],
        };
        assert!(report.contains(&uuid_on_air[..4]));
        assert!(!report.contains(&[0xde, 0xad]));
        let line = report.line();
        assert!(line.contains("address=4438e8239460 address_type=1"));
        assert!(line.contains("anonymous=?"));
        assert!(line.contains("ad=01:06 21:fa21b92f"));
    }
}
