//! Bounded native discovery. Never treats an advertisement as an authenticated identity.
//!
//! Android advertises the control service UUID `c8d5a101-...` in the advertising
//! PDU and its 22-byte announcement (manufacturer `0xFFFF`) in the scan
//! response, and its scanners filter on that service UUID. Windows reports the
//! advertisement and the scan response as separate events, and a passive scan
//! never requests the scan response. The scanner is therefore active and joins
//! the two parts per address (each part at most 2 s old) before applying
//! Android's rules: the service UUID is required (or, behind the compact
//! switch, an iPhone compact UUID), present manufacturer data always wins and
//! a malformed one never falls through to the compact form.
use super::{Apartment, Error, central::read_buffer, probe};
use crate::discovery::Announcement;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, Ordering},
        mpsc::{Receiver, sync_channel},
    },
    time::{Duration, Instant},
};
use windows::{
    Devices::Bluetooth::{Advertisement::*, BluetoothAddressType},
    Foundation::TypedEventHandler,
};

/// `c8d5a101-9f88-4f65-a03d-7f5a2fb921fa` in canonical (big-endian) order.
pub const CONTROL_SERVICE: [u8; 16] = 0xc8d5a101_9f88_4f65_a03d_7f5a2fb921fau128.to_be_bytes();
/// Advertising and scan-response parts are joined only while both are this fresh.
pub const PART_TTL: Duration = Duration::from_secs(2);
pub const MAX_ADDRESSES: usize = 256;
/// A legacy advertising PDU carries at most 31 bytes.
const MAX_SECTION_BYTES: usize = 31;
const MAX_UUIDS: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanOptions {
    /// Rotating discovery IDs: version 5 and compact version 2 become visible.
    pub rotating: bool,
    /// Experimental iPhone compact discovery (off by default on Android).
    pub compact: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Advertisement,
    ScanResponse,
}
/// One received PDU's MeshLive-relevant sections.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sections {
    /// The first `0xFFFF` manufacturer section, any length (a wrong length rejects).
    pub manufacturer: Option<Vec<u8>>,
    pub uuids: Vec<[u8; 16]>,
}

#[derive(Default)]
struct Slot {
    sections: Sections,
    at: Option<Instant>,
}
impl Slot {
    fn fresh(&self, now: Instant) -> Option<&Sections> {
        self.at
            .filter(|at| now.saturating_duration_since(*at) < PART_TTL)
            .map(|_| &self.sections)
    }
}
struct Entry {
    key: (u64, i32),
    touched: Instant,
    advertisement: Slot,
    response: Slot,
}

/// Joins advertisement and scan response per (address, address type).
pub struct Merger {
    options: ScanOptions,
    entries: VecDeque<Entry>,
}
impl Merger {
    pub fn new(options: ScanOptions) -> Self {
        Self {
            options,
            entries: VecDeque::new(),
        }
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn offer(
        &mut self,
        key: (u64, i32),
        part: Part,
        sections: Sections,
        now: Instant,
    ) -> Option<Announcement> {
        self.entries
            .retain(|e| now.saturating_duration_since(e.touched) < PART_TTL);
        let index = match self.entries.iter().position(|e| e.key == key) {
            Some(index) => index,
            None => {
                if self.entries.len() >= MAX_ADDRESSES {
                    self.entries.pop_front();
                }
                self.entries.push_back(Entry {
                    key,
                    touched: now,
                    advertisement: Slot::default(),
                    response: Slot::default(),
                });
                self.entries.len() - 1
            }
        };
        let entry = &mut self.entries[index];
        entry.touched = now;
        let slot = match part {
            Part::Advertisement => &mut entry.advertisement,
            Part::ScanResponse => &mut entry.response,
        };
        slot.sections = sections;
        slot.at = Some(now);
        let parts: Vec<&Sections> = [entry.advertisement.fresh(now), entry.response.fresh(now)]
            .into_iter()
            .flatten()
            .collect();
        decide(self.options, &parts)
    }
}

/// Android's rules on the merged scan record.
fn decide(options: ScanOptions, parts: &[&Sections]) -> Option<Announcement> {
    let uuids = || parts.iter().flat_map(|s| s.uuids.iter());
    let control = uuids().any(|u| *u == CONTROL_SERVICE);
    let compact = if options.compact {
        uuids().find(|u| u[..2] == [0xc8, 0xd6]).copied()
    } else {
        None
    };
    if !control && compact.is_none() {
        return None;
    }
    // The newest part with a manufacturer section decides; the response is newer.
    if let Some(bytes) = parts.iter().rev().find_map(|s| s.manufacturer.as_ref()) {
        return Announcement::decode(bytes, options.rotating).ok();
    }
    Announcement::from_compact(compact?, options.rotating).ok()
}

/// Ephemeral routing hint. Deliberately has no Debug/serialization or persistent address lookup.
#[derive(Clone)]
pub struct Route {
    pub(crate) address: u64,
    pub(crate) address_type: BluetoothAddressType,
}
impl Route {
    /// An in-memory key for "the same advertising address" (the copied-ID
    /// warning compares addresses; it is never logged or stored).
    pub fn address_key(&self) -> u64 {
        self.address
    }
    /// For the explicit, root-approved radio-check tool only: the received
    /// (usually random) address and its type. Never logged by the product.
    pub fn diagnostic_address(&self) -> (u64, i32) {
        (self.address, self.address_type.0)
    }
}
#[derive(Clone)]
pub struct Sighting {
    pub route: Route,
    pub announcement: Announcement,
    pub rssi: i16,
    pub received: Instant,
    /// The report that completed the announcement: Windows advertisement
    /// type (0 connectable undirected, 1 directed, 2 scannable, 3 non
    /// connectable, 4 scan response, 5 extended) and its connectable and
    /// scannable flags. Diagnostic only.
    pub kind: (i32, bool, bool),
}

pub struct Scanner {
    watcher: BluetoothLEAdvertisementWatcher,
    received_token: i64,
    stopped_token: i64,
    receiver: Receiver<Sighting>,
    stopped: Arc<AtomicBool>,
    failure: Arc<AtomicI32>,
    counts: Arc<Mutex<(u64, std::collections::HashMap<u64, u64>)>>,
    started: Instant,
    _apartment: Apartment,
}
impl Scanner {
    /// Explicit worker action; probe gates it before constructing/starting a watcher.
    /// Active scanning sends scan requests from this PC's (OS-chosen) address.
    pub fn start(options: ScanOptions) -> Result<Self, Error> {
        Self::start_with(options, false)
    }
    /// `every_report`: Experimental, off by default. Sets the watcher's
    /// `SignalStrengthFilter.SamplingInterval` to 0, which Microsoft documents
    /// as raising Received for every advertisement instead of a sampled
    /// subset (Microsoft Learn: BluetoothSignalStrengthFilter.SamplingInterval).
    /// Off until the measured report rates of both arms are compared.
    pub fn start_with(options: ScanOptions, every_report: bool) -> Result<Self, Error> {
        let apartment = Apartment::new()?;
        probe().require(false)?;
        let watcher = BluetoothLEAdvertisementWatcher::new()?;
        watcher.SetScanningMode(BluetoothLEScanningMode::Active)?;
        if every_report {
            use windows::core::Interface;
            let filter = windows::Devices::Bluetooth::BluetoothSignalStrengthFilter::new()?;
            let zero = windows::Foundation::PropertyValue::CreateTimeSpan(windows::Foundation::TimeSpan::default())?
                .cast::<windows::Foundation::IReference<windows::Foundation::TimeSpan>>()?;
            filter.SetSamplingInterval(&zero)?;
            watcher.SetSignalStrengthFilter(&filter)?;
        }
        let (sender, receiver) = sync_channel(64);
        let stopped = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(AtomicI32::new(0));
        // Measured, not assumed: WinRT reports every advertisement it hears
        // (no duplicate filter is documented); count reports and addresses.
        let counts = Arc::new(Mutex::new((0u64, std::collections::HashMap::<u64, u64>::new())));
        let counting = counts.clone();
        let stop = stopped.clone();
        let merger = Mutex::new(Merger::new(options));
        let received_token = watcher.Received(&TypedEventHandler::<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementReceivedEventArgs,
        >::new(move |_, args| {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let args = args.ok()?;
            if let (Ok(mut c), Ok(address)) = (counting.lock(), args.BluetoothAddress()) {
                c.0 += 1;
                if c.1.len() < MAX_ADDRESSES * 4 || c.1.contains_key(&address) {
                    *c.1.entry(address).or_insert(0) += 1;
                }
            }
            let ad = args.Advertisement()?;
            let mut sections = Sections::default();
            let manufacturers = ad.GetManufacturerDataByCompanyId(0xffff)?;
            if manufacturers.Size()? > 0 {
                // An oversized section is recorded as malformed, never truncated.
                sections.manufacturer = Some(
                    read_buffer(&manufacturers.GetAt(0)?.Data()?, MAX_SECTION_BYTES)
                        .unwrap_or_default(),
                );
            }
            for uuid in &ad.ServiceUuids()? {
                if sections.uuids.len() == MAX_UUIDS {
                    break;
                }
                sections.uuids.push(uuid.to_u128().to_be_bytes());
            }
            let kind = (
                args.AdvertisementType()?.0,
                args.IsConnectable().unwrap_or(false),
                args.IsScannable().unwrap_or(false),
            );
            let part = if args.AdvertisementType()? == BluetoothLEAdvertisementType::ScanResponse {
                Part::ScanResponse
            } else {
                Part::Advertisement
            };
            let address = args.BluetoothAddress()?;
            let address_type = args.BluetoothAddressType()?;
            let now = Instant::now();
            let decided =
                merger
                    .lock()
                    .unwrap()
                    .offer((address, address_type.0), part, sections, now);
            if let Some(announcement) = decided {
                let _ = sender.try_send(Sighting {
                    route: Route {
                        address,
                        address_type,
                    },
                    announcement,
                    rssi: args.RawSignalStrengthInDBm()?,
                    received: now,
                    kind,
                });
            }
            Ok(())
        }))?;
        let stop = stopped.clone();
        let error = failure.clone();
        let stopped_token = match watcher.Stopped(&TypedEventHandler::<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementWatcherStoppedEventArgs,
        >::new(move |_, args| {
            error.store(args.ok()?.Error()?.0, Ordering::Release);
            stop.store(true, Ordering::Release);
            Ok(())
        })) {
            Ok(token) => token,
            Err(error) => {
                let _ = watcher.RemoveReceived(received_token);
                return Err(error.into());
            }
        };
        let scanner = Self {
            watcher,
            received_token,
            stopped_token,
            receiver,
            stopped,
            failure,
            counts,
            started: Instant::now(),
            _apartment: apartment,
        };
        scanner.watcher.Start()?;
        Ok(scanner)
    }
    /// One log line of what the WinRT watcher actually did (stack facts):
    /// the scanning mode it reports, extended advertisements, the signal
    /// filter's sampling interval, and the measured report rate per address
    /// (a high rate means no duplicate filtering).
    pub fn facts(&self) -> String {
        let seconds = self.started.elapsed().as_secs_f64().max(0.001);
        let (reports, addresses, busiest) = self
            .counts
            .lock()
            .map(|c| (c.0, c.1.len(), c.1.values().copied().max().unwrap_or(0)))
            .unwrap_or((0, 0, 0));
        let mode = self.watcher.ScanningMode().map(|m| m.0).unwrap_or(-1);
        let extended = self.watcher.AllowExtendedAdvertisements().map_or("unavailable".to_owned(), |e| e.to_string());
        let sampling_ms = self
            .watcher
            .SignalStrengthFilter()
            .and_then(|f| f.SamplingInterval())
            .and_then(|i| i.Value())
            .map_or("none".to_owned(), |t| (t.Duration / 10_000).to_string());
        format!(
            "fact=scanner mode={mode} (0 passive, 1 active) extended={extended} sampling_ms={sampling_ms} seconds={seconds:.1} reports={reports} addresses={addresses} reports_per_address_per_s={:.2} busiest_address_per_s={:.2}",
            if addresses == 0 { 0.0 } else { reports as f64 / addresses as f64 / seconds },
            busiest as f64 / seconds
        )
    }
    pub fn receive(&self, deadline: Instant, cancelled: &AtomicBool) -> Result<Sighting, Error> {
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            if self.stopped.load(Ordering::Acquire) {
                return Err(Error::ScanStopped(self.failure.load(Ordering::Acquire)));
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            match self.receiver.recv_timeout(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(50)),
            ) {
                Ok(sighting) => return Ok(sighting),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Error::Cancelled);
                }
            }
        }
    }
}
impl Drop for Scanner {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.watcher.Stop();
        let _ = self.watcher.RemoveReceived(self.received_token);
        let _ = self.watcher.RemoveStopped(self.stopped_token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{ACCEPTING, CALL, GATT};
    fn call_offer(version: u8) -> [u8; 22] {
        let mut raw = [0; 22];
        raw[0] = version;
        raw[1..7].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        raw[7] = ACCEPTING | CALL | GATT;
        raw[8] = 3;
        raw
    }
    fn adv(uuids: &[[u8; 16]]) -> Sections {
        Sections {
            manufacturer: None,
            uuids: uuids.to_vec(),
        }
    }
    fn response(raw: &[u8]) -> Sections {
        Sections {
            manufacturer: Some(raw.to_vec()),
            uuids: vec![],
        }
    }
    const COMPACT: [u8; 16] = 0xc8d64010_2030_4050_60c0_000000000000u128.to_be_bytes();
    #[test]
    fn an_android_advertisement_and_its_scan_response_join_into_one_announcement() {
        let t = Instant::now();
        let mut m = Merger::new(ScanOptions::default());
        // The advertising PDU alone (service UUID, no announcement) waits.
        assert!(
            m.offer((1, 0), Part::Advertisement, adv(&[CONTROL_SERVICE]), t)
                .is_none()
        );
        let got = m
            .offer(
                (1, 0),
                Part::ScanResponse,
                response(&call_offer(2)),
                t + Duration::from_millis(30),
            )
            .unwrap();
        assert_eq!(got.version, 2);
        assert_eq!(got.prefix, [1, 2, 3, 4, 5, 6]);
        // The reverse order works too, and another address type is a separate record.
        let mut m = Merger::new(ScanOptions::default());
        assert!(
            m.offer((2, 1), Part::ScanResponse, response(&call_offer(2)), t)
                .is_none()
        );
        assert!(
            m.offer((2, 0), Part::Advertisement, adv(&[CONTROL_SERVICE]), t)
                .is_none()
        );
        assert!(
            m.offer((2, 1), Part::Advertisement, adv(&[CONTROL_SERVICE]), t)
                .is_some()
        );
        // A part older than 2 s is never joined.
        let mut m = Merger::new(ScanOptions::default());
        m.offer((3, 0), Part::Advertisement, adv(&[CONTROL_SERVICE]), t);
        assert!(
            m.offer(
                (3, 0),
                Part::ScanResponse,
                response(&call_offer(2)),
                t + PART_TTL
            )
            .is_none()
        );
    }
    #[test]
    fn android_filter_rules_apply_to_the_merged_record() {
        let t = Instant::now();
        // A foreign 0xFFFF section without the MeshLive service UUID is ignored.
        let mut m = Merger::new(ScanOptions {
            rotating: true,
            compact: true,
        });
        assert!(
            m.offer((1, 0), Part::Advertisement, response(&call_offer(2)), t)
                .is_none()
        );
        // Version 5 needs the rotating switch.
        let mut m = Merger::new(ScanOptions::default());
        m.offer((1, 0), Part::Advertisement, adv(&[CONTROL_SERVICE]), t);
        assert!(
            m.offer((1, 0), Part::ScanResponse, response(&call_offer(5)), t)
                .is_none()
        );
        let mut m = Merger::new(ScanOptions {
            rotating: true,
            compact: false,
        });
        m.offer((1, 0), Part::Advertisement, adv(&[CONTROL_SERVICE]), t);
        assert_eq!(
            m.offer((1, 0), Part::ScanResponse, response(&call_offer(5)), t)
                .unwrap()
                .version,
            5
        );
        // Compact UUIDs only behind the switch; manufacturer data wins; a malformed
        // manufacturer section never falls through to the compact form.
        let mut m = Merger::new(ScanOptions::default());
        assert!(
            m.offer((4, 0), Part::Advertisement, adv(&[COMPACT]), t)
                .is_none()
        );
        let mut m = Merger::new(ScanOptions {
            rotating: false,
            compact: true,
        });
        assert_eq!(
            m.offer((4, 0), Part::Advertisement, adv(&[COMPACT]), t)
                .unwrap()
                .flags
                & CALL,
            CALL
        );
        let mut both = adv(&[COMPACT, CONTROL_SERVICE]);
        both.manufacturer = Some(call_offer(2).to_vec());
        assert_eq!(
            m.offer((5, 0), Part::Advertisement, both.clone(), t)
                .unwrap()
                .prefix,
            [1, 2, 3, 4, 5, 6]
        );
        both.manufacturer = Some(call_offer(2)[..21].to_vec());
        assert!(
            m.offer((6, 0), Part::Advertisement, both.clone(), t)
                .is_none()
        );
        both.manufacturer = Some(vec![]);
        assert!(m.offer((7, 0), Part::Advertisement, both, t).is_none());
        let mut bad = call_offer(2);
        bad[0] = 9;
        m.offer((8, 0), Part::Advertisement, adv(&[CONTROL_SERVICE]), t);
        assert!(
            m.offer((8, 0), Part::ScanResponse, response(&bad), t)
                .is_none()
        );
    }
    #[test]
    fn the_join_table_is_bounded_and_expires() {
        let t = Instant::now();
        let mut m = Merger::new(ScanOptions::default());
        for address in 0..(MAX_ADDRESSES as u64 + 10) {
            m.offer(
                (address, 0),
                Part::Advertisement,
                adv(&[CONTROL_SERVICE]),
                t,
            );
        }
        assert_eq!(m.len(), MAX_ADDRESSES);
        // The oldest addresses were evicted; their scan responses no longer join.
        assert!(
            m.offer((0, 0), Part::ScanResponse, response(&call_offer(2)), t)
                .is_none()
        );
        assert!(
            m.offer(
                (MAX_ADDRESSES as u64 + 9, 0),
                Part::ScanResponse,
                response(&call_offer(2)),
                t
            )
            .is_some()
        );
        m.offer((1_000, 0), Part::Advertisement, adv(&[]), t + PART_TTL);
        assert_eq!(m.len(), 1);
    }
}
