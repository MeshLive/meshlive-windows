//! GATT carrier only. Discovery hints and carrier connections are never authenticated peers.
pub mod advertise;
pub mod carrier;
pub mod central;
pub mod gate;
pub mod gr1;
pub mod pacer;
pub mod radio_watch;
pub mod peripheral;
pub mod scan;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows::{
    Devices::{Bluetooth::BluetoothAdapter, Radios::RadioState},
    Win32::System::Com::{CO_MTA_USAGE_COOKIE, CoDecrementMTAUsage, CoIncrementMTAUsage},
    Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Radio {
    On,
    Off,
    Disabled,
    Unknown,
    Missing,
    Denied,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub radio: Radio,
    pub low_energy: Option<bool>,
    pub central: Option<bool>,
    pub peripheral: Option<bool>,
    /// Windows 10 2004+ only; `None` on older builds. Informational: the gate
    /// relies on an actual advertisement start, never on this flag.
    pub extended_advertising: Option<bool>,
    pub max_advertisement_bytes: Option<u32>,
    pub error: Option<i32>,
}

impl Capabilities {
    pub fn status(&self) -> &'static str {
        match self.radio {
            Radio::On => "Bluetooth is on",
            Radio::Off => "Bluetooth is off",
            Radio::Disabled => "Bluetooth is disabled",
            Radio::Missing => "No Bluetooth adapter found",
            Radio::Denied => "Bluetooth access needs user permission",
            Radio::Unknown | Radio::Unavailable => "Bluetooth status is unavailable",
        }
    }

    /// A fresh snapshot is required before each explicit operation; never changes radio state.
    pub fn require(&self, peripheral: bool) -> Result<(), Error> {
        if self.radio != Radio::On {
            return Err(Error::Radio(self.radio));
        }
        if self.low_energy != Some(true) {
            return Err(Error::Unsupported);
        }
        if (if peripheral {
            self.peripheral
        } else {
            self.central
        }) != Some(true)
        {
            return Err(Error::Unsupported);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Gatt(i32),
    Bluetooth(i32),
    ScanStopped(i32),
    Radio(Radio),
    Unsupported,
    /// The advertisement start reported Aborted.
    AdvertisingAborted,
    /// Started without all advertisement data: the announcement is not on air.
    AdvertisingIncomplete,
    Timeout,
    Cancelled,
    Native(i32),
    /// CF-PLAT-05: Windows holds a pairing with this phone and its services
    /// stayed unreachable for the whole setup budget: the phone no longer
    /// accepts that bond (the link drops at encryption).
    BondMismatch,
}
impl Error {
    /// What a failed explicit advertisement start proves for the feature gate.
    /// A cancellation proves nothing; a refused role or radio state is already
    /// visible in the capability snapshot.
    pub fn refusal(self) -> Option<gate::Refusal> {
        match self {
            Self::AdvertisingAborted => Some(gate::Refusal::Aborted),
            Self::AdvertisingIncomplete => Some(gate::Refusal::Incomplete),
            Self::Timeout => Some(gate::Refusal::Timeout),
            Self::Native(code) | Self::Bluetooth(code) | Self::Gatt(code) => {
                Some(gate::Refusal::Native(code))
            }
            Self::Unsupported | Self::Radio(_) | Self::Cancelled | Self::ScanStopped(_) | Self::BondMismatch => None,
        }
    }
}

impl From<windows::core::Error> for Error {
    fn from(error: windows::core::Error) -> Self {
        Self::Native(error.code().0)
    }
}

/// Keeps cached WinRT factories valid between worker apartments. The app
/// owns this before creating workers and drops it after they have stopped.
pub struct MtaUsage(CO_MTA_USAGE_COOKIE);
impl MtaUsage {
    pub fn for_process() -> Result<Option<Self>, Error> {
        if std::env::var("MESHLIVE_MTA_LIFETIME").as_deref() != Ok("1") { return Ok(None); }
        Self::acquire().map(Some)
    }
    fn acquire() -> Result<Self, Error> { unsafe { CoIncrementMTAUsage().map(Self).map_err(Error::from) } }
}
impl Drop for MtaUsage {
    fn drop(&mut self) { unsafe { let _ = CoDecrementMTAUsage(self.0); } }
}

pub(crate) struct Apartment(std::marker::PhantomData<std::rc::Rc<()>>);
impl Apartment {
    pub(crate) fn new() -> Result<Self, Error> {
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
        }
        Ok(Self(std::marker::PhantomData))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}

/// Only for the dedicated Bluetooth worker, never the UI thread. Cancellation is fail-closed.
pub(crate) fn wait<T: windows::core::RuntimeType + 'static>(
    op: windows_future::IAsyncOperation<T>,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<T, Error> {
    wait_with(op, deadline, || cancelled.load(Ordering::Acquire))
}

pub(crate) fn wait_with<T: windows::core::RuntimeType + 'static>(
    op: windows_future::IAsyncOperation<T>,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<T, Error> {
    if op.Status()? == windows_future::AsyncStatus::Started {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        op.SetCompleted(&windows_future::AsyncOperationCompletedHandler::new(
            move |_, _| {
                let _ = sender.try_send(());
                Ok(())
            },
        ))?;
        loop {
            if cancelled() || Instant::now() >= deadline {
                let _ = op.Cancel();
                return Err(if cancelled() {
                    Error::Cancelled
                } else {
                    Error::Timeout
                });
            }
            match receiver.recv_timeout(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(50)),
            ) {
                Ok(()) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = op.Cancel();
                    return Err(Error::Cancelled);
                }
            }
        }
    }
    if cancelled() || Instant::now() >= deadline {
        let _ = op.Cancel();
        return Err(if cancelled() {
            Error::Cancelled
        } else {
            Error::Timeout
        });
    }
    let result = op.GetResults().map_err(Error::from);
    let _ = op.Close();
    result
}

/// The default adapter's device interface ID (e.g. `\\?\USB#VID_8087&PID_0026#...`),
/// read only, for the Design QW platform allowlist and the logs.
pub fn adapter_device_id() -> Option<String> {
    let _apartment = Apartment::new().ok()?;
    let cancelled = AtomicBool::new(false);
    let adapter = wait(BluetoothAdapter::GetDefaultAsync().ok()?, Instant::now() + Duration::from_secs(5), &cancelled).ok()?;
    adapter.DeviceId().ok().map(|id| id.to_string())
}

/// Read-only WinRT probe. No RequestAccess, SetState, pairing, scanning or advertising calls.
pub fn probe() -> Capabilities {
    let mut result = Capabilities {
        radio: Radio::Unavailable,
        low_energy: None,
        central: None,
        peripheral: None,
        extended_advertising: None,
        max_advertisement_bytes: None,
        error: None,
    };
    let mut read = || -> Result<Capabilities, Error> {
        let _apartment = Apartment::new()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let cancelled = AtomicBool::new(false);
        let adapter = wait(BluetoothAdapter::GetDefaultAsync()?, deadline, &cancelled)?;
        result.low_energy = Some(adapter.IsLowEnergySupported()?);
        result.central = Some(adapter.IsCentralRoleSupported()?);
        result.peripheral = Some(adapter.IsPeripheralRoleSupported()?);
        // Newer adapter interface; its absence (1809-1909) is not an error.
        result.extended_advertising = adapter.IsExtendedAdvertisingSupported().ok();
        result.max_advertisement_bytes = adapter.MaxAdvertisementDataLength().ok();
        let radio = wait(adapter.GetRadioAsync()?, deadline, &cancelled)?;
        result.radio = match radio.State()? {
            RadioState::On => Radio::On,
            RadioState::Off => Radio::Off,
            RadioState::Disabled => Radio::Disabled,
            _ => Radio::Unknown,
        };
        Ok(result)
    };
    match read() {
        Ok(value) => value,
        Err(error) => {
            if let Error::Native(code) = error {
                result.error = Some(code);
                result.radio = match code as u32 {
                    0x80070005 => Radio::Denied,
                    0x80004003 if result.low_energy.is_none() => Radio::Missing,
                    _ => Radio::Unavailable,
                };
            }
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_mta_keeps_native_factories_valid_between_worker_owners() {
        let _usage = MtaUsage::acquire().unwrap();
        for expected in 7..10 {
            assert_eq!(std::thread::spawn(move || {
                let _apartment = Apartment::new().unwrap();
                use windows::core::Interface;
                windows::Foundation::PropertyValue::CreateUInt32(expected).unwrap()
                    .cast::<windows::Foundation::IPropertyValue>().unwrap().GetUInt32().unwrap()
            }).join().unwrap(), expected);
        }
    }
    #[test]
    fn completion_wait_handles_ready_pending_cancelled_and_expired_operations() {
        let _apartment = Apartment::new().unwrap();
        let ready = windows_future::IAsyncOperation::<u32>::ready(Ok(7));
        assert_eq!(
            wait(
                ready,
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(false)
            ),
            Ok(7)
        );
        let (release, parked) = std::sync::mpsc::channel();
        let pending = windows_future::IAsyncOperation::<u32>::spawn(move || {
            parked.recv().map_err(|_| {
                windows::core::Error::from_hresult(windows::core::HRESULT(0x80004004u32 as i32))
            })?;
            Ok(9)
        });
        let worker = std::thread::spawn(move || {
            // windows-future intentionally does not implement the Completed getter.
            std::thread::sleep(Duration::from_millis(20));
            let _ = release.send(());
        });
        assert_eq!(
            wait(
                pending,
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(false)
            ),
            Ok(9)
        );
        worker.join().unwrap();
        for (cancelled, deadline, expected) in [
            (
                true,
                Instant::now() + Duration::from_secs(1),
                Error::Cancelled,
            ),
            (false, Instant::now(), Error::Timeout),
        ] {
            assert_eq!(
                wait(
                    windows_future::IAsyncOperation::<u32>::ready(Ok(1)),
                    deadline,
                    &AtomicBool::new(cancelled)
                ),
                Err(expected)
            );
        }
    }
    #[test]
    fn advertisement_start_errors_map_to_gate_refusals_and_cancellation_proves_nothing() {
        use gate::Refusal;
        assert_eq!(Error::AdvertisingAborted.refusal(), Some(Refusal::Aborted));
        assert_eq!(
            Error::AdvertisingIncomplete.refusal(),
            Some(Refusal::Incomplete)
        );
        assert_eq!(Error::Timeout.refusal(), Some(Refusal::Timeout));
        assert_eq!(Error::Bluetooth(3).refusal(), Some(Refusal::Native(3)));
        for error in [
            Error::Cancelled,
            Error::Unsupported,
            Error::Radio(Radio::Off),
        ] {
            assert_eq!(error.refusal(), None);
        }
    }
    #[test]
    fn radio_and_each_role_must_be_explicitly_available() {
        let ready = Capabilities {
            radio: Radio::On,
            low_energy: Some(true),
            central: Some(true),
            peripheral: Some(true),
            extended_advertising: None,
            max_advertisement_bytes: None,
            error: None,
        };
        assert_eq!(ready.require(false), Ok(()));
        assert_eq!(ready.require(true), Ok(()));
        for radio in [
            Radio::Off,
            Radio::Disabled,
            Radio::Unknown,
            Radio::Missing,
            Radio::Denied,
            Radio::Unavailable,
        ] {
            let blocked = Capabilities { radio, ..ready };
            assert!(blocked.require(false).is_err());
            assert!(blocked.require(true).is_err());
        }
        let no_peripheral = Capabilities {
            peripheral: Some(false),
            ..ready
        };
        assert_eq!(no_peripheral.require(false), Ok(()));
        assert_eq!(no_peripheral.require(true), Err(Error::Unsupported));
        assert!(
            Capabilities {
                low_energy: None,
                ..ready
            }
            .require(false)
            .is_err()
        );
        assert!(
            Capabilities {
                central: None,
                ..ready
            }
            .require(false)
            .is_err()
        );
    }
}
