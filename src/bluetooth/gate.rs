//! Capability-gated features. In MeshLive the callee and the viewer are the
//! advertising GATT server; callers and sources connect as central. A PC whose
//! adapter cannot advertise can therefore only place outgoing calls. Every
//! refused feature carries a plain reason; nothing retries in the background
//! and no other transport, fixed ID or cleartext path is substituted.
use super::{Capabilities, Radio};

/// The exact text shown wherever a peripheral feature is unavailable.
pub const NO_PERIPHERAL: &str = "This PC's Bluetooth adapter can't advertise, so phones can't connect to it. Calling a phone still works. A Bluetooth 5 USB adapter that supports peripheral mode fixes this.";
pub const NO_LOW_ENERGY: &str =
    "This PC's Bluetooth adapter doesn't support Bluetooth Low Energy, which MeshLive needs.";
pub const NO_CENTRAL: &str =
    "This PC's Bluetooth adapter can't connect to phones, so calls and streams are unavailable.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// Calling a phone: central only.
    CallOut,
    /// Receiving calls: the callee advertises its call offer.
    ReceiveCalls,
    /// Watching: the viewer advertises its viewer request and the source connects to it.
    Watch,
    /// Going live: the stream offer is advertised (design decision 2026-10-19).
    GoLive,
    /// Being visible to contacts: call-ready presence is an advertisement.
    VisibleToContacts,
}
impl Feature {
    pub const ALL: [Self; 5] = [
        Self::CallOut,
        Self::ReceiveCalls,
        Self::Watch,
        Self::GoLive,
        Self::VisibleToContacts,
    ];
    pub fn needs_peripheral(self) -> bool {
        self != Self::CallOut
    }
    /// A source or viewer also connects as central for its side of the link.
    fn needs_central(self) -> bool {
        matches!(self, Self::CallOut | Self::Watch | Self::GoLive)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::CallOut => "Call a phone",
            Self::ReceiveCalls => "Allow calls",
            Self::Watch => "Watch",
            Self::GoLive => "Go live",
            Self::VisibleToContacts => "Visible to contacts",
        }
    }
}

/// Why the first real advertisement start did not prove the peripheral role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// GattServiceProviderAdvertisementStatus::Aborted.
    Aborted,
    /// Started without all advertisement data: the announcement is not on air.
    Incomplete,
    /// The start did not report Started within its bound.
    Timeout,
    /// The OS refused the operation itself (HRESULT or Bluetooth error).
    Native(i32),
}

/// What the first explicit advertisement start proved. Role flags alone are
/// not proof; only an actual start is. A refusal latches until the user asks again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PeripheralProof {
    #[default]
    Untested,
    Started,
    Refused(Refusal),
}
impl PeripheralProof {
    /// Records one start outcome. A refusal is never overwritten by a later
    /// automatic attempt: only `recheck_by_user` clears it.
    pub fn record(&mut self, outcome: Result<(), Refusal>) {
        if matches!(self, Self::Refused(_)) {
            return;
        }
        *self = match outcome {
            Ok(()) => Self::Started,
            Err(refusal) => Self::Refused(refusal),
        };
    }
    /// An explicit user action ("Check again", a new adapter) allows one new start.
    pub fn recheck_by_user(&mut self) {
        *self = Self::Untested;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Available,
    /// The adapter claims the role; the first start when the user turns the
    /// feature on decides. The UI may offer it with that caveat.
    NeedsAdvertisingCheck,
    Unavailable(&'static str),
}

pub fn availability(caps: &Capabilities, proof: PeripheralProof, feature: Feature) -> Availability {
    if caps.radio != Radio::On {
        return Availability::Unavailable(caps.status());
    }
    if caps.low_energy != Some(true) {
        return Availability::Unavailable(NO_LOW_ENERGY);
    }
    if feature.needs_central() && caps.central != Some(true) {
        return Availability::Unavailable(NO_CENTRAL);
    }
    if !feature.needs_peripheral() {
        return Availability::Available;
    }
    match (caps.peripheral, proof) {
        (Some(false), _) | (_, PeripheralProof::Refused(_)) => {
            Availability::Unavailable(NO_PERIPHERAL)
        }
        (Some(true), PeripheralProof::Started) => Availability::Available,
        _ => Availability::NeedsAdvertisingCheck,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn caps(central: Option<bool>, peripheral: Option<bool>) -> Capabilities {
        Capabilities {
            radio: Radio::On,
            low_energy: Some(true),
            central,
            peripheral,
            extended_advertising: None,
            max_advertisement_bytes: None,
            error: None,
        }
    }
    #[test]
    fn an_adapter_without_the_peripheral_role_keeps_only_outgoing_calls_with_the_plain_reason() {
        for proof in [
            PeripheralProof::Untested,
            PeripheralProof::Started,
            PeripheralProof::Refused(Refusal::Aborted),
        ] {
            let c = caps(Some(true), Some(false));
            assert_eq!(
                availability(&c, proof, Feature::CallOut),
                Availability::Available
            );
            for feature in Feature::ALL.into_iter().filter(|f| f.needs_peripheral()) {
                assert_eq!(
                    availability(&c, proof, feature),
                    Availability::Unavailable(NO_PERIPHERAL),
                    "{feature:?}"
                );
            }
        }
        assert!(NO_PERIPHERAL.contains("Calling a phone still works"));
    }
    #[test]
    fn the_role_flag_is_not_proof_and_a_failed_start_latches_until_the_user_asks() {
        let c = caps(Some(true), Some(true));
        let mut proof = PeripheralProof::default();
        assert_eq!(
            availability(&c, proof, Feature::Watch),
            Availability::NeedsAdvertisingCheck
        );
        proof.record(Err(Refusal::Incomplete));
        for feature in Feature::ALL.into_iter().filter(|f| f.needs_peripheral()) {
            assert_eq!(
                availability(&c, proof, feature),
                Availability::Unavailable(NO_PERIPHERAL)
            );
        }
        assert_eq!(
            availability(&c, proof, Feature::CallOut),
            Availability::Available
        );
        // No hidden retry: a later automatic success cannot clear the refusal.
        proof.record(Ok(()));
        assert_eq!(proof, PeripheralProof::Refused(Refusal::Incomplete));
        proof.recheck_by_user();
        proof.record(Ok(()));
        for feature in Feature::ALL {
            assert_eq!(availability(&c, proof, feature), Availability::Available);
        }
        assert_eq!(
            availability(
                &caps(Some(true), None),
                PeripheralProof::Untested,
                Feature::GoLive
            ),
            Availability::NeedsAdvertisingCheck
        );
    }
    #[test]
    fn radio_low_energy_and_central_gates_come_first() {
        let mut c = caps(Some(true), Some(true));
        c.radio = Radio::Off;
        for feature in Feature::ALL {
            assert_eq!(
                availability(&c, PeripheralProof::Started, feature),
                Availability::Unavailable("Bluetooth is off")
            );
        }
        let mut c = caps(Some(true), Some(true));
        c.low_energy = Some(false);
        assert_eq!(
            availability(&c, PeripheralProof::Started, Feature::CallOut),
            Availability::Unavailable(NO_LOW_ENERGY)
        );
        let c = caps(Some(false), Some(true));
        for feature in [Feature::CallOut, Feature::Watch, Feature::GoLive] {
            assert_eq!(
                availability(&c, PeripheralProof::Started, feature),
                Availability::Unavailable(NO_CENTRAL)
            );
        }
        assert_eq!(
            availability(&c, PeripheralProof::Started, Feature::ReceiveCalls),
            Availability::Available
        );
    }
}
