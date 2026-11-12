//! The Wi-Fi Direct legacy group publisher. Event driven, never blocks the
//! caller; the publisher and its apartment live and die on the worker thread.

use std::sync::mpsc;
use windows::{
    Devices::WiFiDirect::{
        WiFiDirectAdvertisementListenStateDiscoverability, WiFiDirectAdvertisementPublisher,
        WiFiDirectAdvertisementPublisherStatus, WiFiDirectAdvertisementPublisherStatusChangedEventArgs,
    },
    Foundation::TypedEventHandler,
    Security::Credentials::PasswordCredential,
    core::HSTRING,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupState {
    Pending,
    Started,
    /// `WiFiDirectError`: 1 RadioNotAvailable, 2 ResourceInUse, 0 other.
    Aborted(i32),
    Stopped,
}

/// Pure mapping of a status/error event. `None` means no state change (Created).
pub fn state_of(status: i32, error: i32) -> Option<GroupState> {
    if status == WiFiDirectAdvertisementPublisherStatus::Started.0 {
        Some(GroupState::Started)
    } else if status == WiFiDirectAdvertisementPublisherStatus::Stopped.0 {
        Some(GroupState::Stopped)
    } else if status == WiFiDirectAdvertisementPublisherStatus::Aborted.0 {
        Some(GroupState::Aborted(error))
    } else {
        None
    }
}

pub struct HostGroup {
    publisher: WiFiDirectAdvertisementPublisher,
    token: i64,
    events: mpsc::Receiver<(i32, i32)>,
    state: GroupState,
    stopped: bool,
    // `!Send`; declared last so it is dropped after the publisher.
    _apartment: crate::bluetooth::Apartment,
}

impl HostGroup {
    /// Configures and calls `Start()`; returns at once. The status arrives on `poll`.
    pub fn start(ssid: &str, psk: &str) -> Result<Self, crate::bluetooth::Error> {
        let apartment = crate::bluetooth::Apartment::new()?;
        let publisher = WiFiDirectAdvertisementPublisher::new()?;
        let advertisement = publisher.Advertisement()?;
        advertisement.SetIsAutonomousGroupOwnerEnabled(true)?;
        advertisement.SetListenStateDiscoverability(WiFiDirectAdvertisementListenStateDiscoverability::None)?;
        let legacy = advertisement.LegacySettings()?;
        legacy.SetIsEnabled(true)?;
        legacy.SetSsid(&HSTRING::from(ssid))?;
        let credential = PasswordCredential::CreatePasswordCredential(
            &HSTRING::from("meshlive"),
            &HSTRING::from("wifi"),
            &HSTRING::from(psk),
        )?;
        legacy.SetPassphrase(&credential)?;
        let (sender, events) = mpsc::channel();
        let token = publisher.StatusChanged(&TypedEventHandler::<
            WiFiDirectAdvertisementPublisher,
            WiFiDirectAdvertisementPublisherStatusChangedEventArgs,
        >::new(move |_, args| {
            if let Some(args) = args.as_ref() {
                let (status, error) = (args.Status()?.0, args.Error()?.0);
                let _ = sender.send((status, error));
            }
            Ok(())
        }))?;
        publisher.Start()?;
        Ok(Self {
            publisher,
            token,
            events,
            state: GroupState::Pending,
            stopped: false,
            _apartment: apartment,
        })
    }

    /// Drains the status channel. `Started` stays `Started`; `Aborted`/`Stopped` are terminal.
    pub fn poll(&mut self) -> GroupState {
        while let Ok((status, error)) = self.events.try_recv() {
            if let Some(state) = state_of(status, error) {
                self.state = state;
            }
        }
        self.state
    }

    /// `Stop()` and remove the handler; idempotent.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let _ = self.publisher.Stop();
        let _ = self.publisher.RemoveStatusChanged(self.token);
        self.state = GroupState::Stopped;
    }
}

impl Drop for HostGroup {
    fn drop(&mut self) {
        self.stop();
        // The publisher is dropped before the apartment by field order.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_events_map_to_group_states() {
        assert_eq!(state_of(0, 0), None);
        assert_eq!(state_of(1, 0), Some(GroupState::Started));
        assert_eq!(state_of(2, 0), Some(GroupState::Stopped));
        assert_eq!(state_of(3, 2), Some(GroupState::Aborted(2)));
    }
}
