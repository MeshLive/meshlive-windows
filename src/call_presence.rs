//! Phase 2 presence leases inside an answered call (PROTOCOL "Recognition and
//! presence leases"; Android `newPresenceSession`, `startPresenceSession`,
//! `runPresenceActions`, `handleDiscoveryContent`).
//!
//! The exchange always judges the peer's REAL identity: the handshake key for
//! a real-key peer (a contact call), the REVEALED identity for a BMH3
//! pseudonym (a default Nearby call), never the throwaway (Android F10: a
//! lease from a revealed verified contact was rejected because the throwaway
//! was judged). Eligibility is decided when the identity is known and
//! re-checked at every send; nothing is sent before the call is answered
//! (RD6); an OFFER that arrives earlier is recorded. Only after both OFFERs
//! and with the peer verified right now is a received lease stored.
use crate::{
    presence::{self, Action, Record, Session},
    verify_scan::PresenceHooks,
};

pub struct CallPresence {
    session: Session,
    /// The peer's real identity: (node ID, fingerprint).
    peer: Option<([u8; 16], [u8; 32])>,
    answered: bool,
    /// The peer's OFFER arrived before its identity was judged.
    early_offer: bool,
    pub log: Vec<String>,
    pub lease_stored: bool,
}
impl Default for CallPresence {
    fn default() -> Self {
        Self::new()
    }
}
impl CallPresence {
    /// Not eligible until the peer's real identity is known.
    pub fn new() -> Self {
        Self {
            session: Session::new(false),
            peer: None,
            answered: false,
            early_offer: false,
            log: Vec::new(),
            lease_stored: false,
        }
    }
    /// The peer's real identity is known: the handshake peer when it was not
    /// a pseudonym, or the identity its verified REVEAL proved. Returns BDX1
    /// records to send (only when the call is already answered).
    pub fn identified(&mut self, node: [u8; 16], fingerprint: [u8; 32], hooks: &mut dyn PresenceHooks) -> Vec<Vec<u8>> {
        if self.peer.is_some() {
            return Vec::new();
        }
        self.peer = Some((node, fingerprint));
        let reason = hooks.ineligible(&node, &fingerprint);
        self.log.push(match reason {
            None => "presence eligible".into(),
            Some(r) => format!("presence ineligible reason={r}"),
        });
        self.session = Session::new(reason.is_none());
        let mut out = Vec::new();
        if std::mem::take(&mut self.early_offer) {
            out.extend(self.on_offer_or_lease(&Record::Offer, hooks));
        }
        if self.answered {
            let actions = self.session.start();
            out.extend(self.run(actions, hooks));
        }
        out
    }
    /// The call was answered (caller: the peer's Accept; callee: own Accept).
    pub fn answered(&mut self, hooks: &mut dyn PresenceHooks) -> Vec<Vec<u8>> {
        if self.answered {
            return Vec::new();
        }
        self.answered = true;
        if self.peer.is_none() {
            return Vec::new();
        }
        let actions = self.session.start();
        self.run(actions, hooks)
    }
    /// A received BDX1 OFFER or LEASE (`CallSession::take_presence_records`).
    pub fn on_record(&mut self, payload: &[u8], hooks: &mut dyn PresenceHooks) -> Vec<Vec<u8>> {
        let Some(record) = Record::decode(payload) else {
            self.log.push("lease_rejected reason=undecodable".into());
            return Vec::new();
        };
        if self.peer.is_none() {
            // Before the reveal: an OFFER is kept for the judged identity; a lease cannot be judged.
            match record {
                Record::Offer if !self.early_offer => self.early_offer = true,
                _ => self.log.push("lease_rejected reason=before_identity".into()),
            }
            return Vec::new();
        }
        self.on_offer_or_lease(&record, hooks)
    }
    fn on_offer_or_lease(&mut self, record: &Record, hooks: &mut dyn PresenceHooks) -> Vec<Vec<u8>> {
        let Some((node, fingerprint)) = self.peer else {
            return Vec::new();
        };
        let verified = hooks.verified_now(&node, &fingerprint);
        let actions = self.session.on_record(record, verified);
        self.run(actions, hooks)
    }
    fn run(&mut self, actions: Vec<Action>, hooks: &mut dyn PresenceHooks) -> Vec<Vec<u8>> {
        let Some((node, fingerprint)) = self.peer else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for action in actions {
            match action {
                Action::SendOffer => match hooks.ineligible(&node, &fingerprint) {
                    None => {
                        out.push(presence::offer().to_vec());
                        self.log.push("offer_sent".into());
                    }
                    Some(reason) => {
                        self.session.offer_withheld();
                        self.log.push(format!("offer_withheld reason={reason}"));
                    }
                },
                Action::SendLease => match hooks.deliver_lease(&node, &fingerprint) {
                    Some(lease) => {
                        out.push(lease.encode());
                        self.log.push(format!("lease_sent generation={}", lease.generation));
                    }
                    None => self.log.push("lease_withheld reason=not_deliverable".into()),
                },
                Action::StoreLease(lease) => {
                    let stored = hooks.store_lease(&node, &lease);
                    self.lease_stored |= stored;
                    self.log.push(format!("lease_stored changed={stored} generation={}", lease.generation));
                }
                Action::Reject(reason) => self.log.push(format!("lease_rejected reason={reason}")),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presence::Lease;

    /// Verified contacts by fingerprint; leases out and in recorded.
    struct Hooks {
        verified: Vec<[u8; 32]>,
        delivered: u32,
        stored: Vec<Lease>,
    }
    impl PresenceHooks for Hooks {
        fn ineligible(&mut self, _: &[u8; 16], peer: &[u8; 32]) -> Option<&'static str> {
            (!self.verified.contains(peer)).then_some("peer_unverified")
        }
        fn deliver_lease(&mut self, _: &[u8; 16], peer: &[u8; 32]) -> Option<Lease> {
            self.verified.contains(peer).then(|| {
                self.delivered += 1;
                lease()
            })
        }
        fn verified_now(&mut self, _: &[u8; 16], peer: &[u8; 32]) -> bool {
            self.verified.contains(peer)
        }
        fn store_lease(&mut self, _: &[u8; 16], lease: &Lease) -> bool {
            self.stored.push(lease.clone());
            true
        }
    }
    fn lease() -> Lease {
        Lease {
            generation: 1,
            sender_epoch: 2_000_000,
            first_day: 2962,
            keys: vec![[1; 32]],
        }
    }
    const REAL: ([u8; 16], [u8; 32]) = ([1; 16], [1; 32]);
    const THROWAWAY: ([u8; 16], [u8; 32]) = ([9; 16], [9; 32]);

    #[test]
    fn a_revealed_verified_contacts_lease_is_stored_and_never_judged_on_the_throwaway() {
        // Android Bmh3PseudonymTest.aRevealedVerifiedContactsLeaseIsStoredAndAnUnrevealedOnesIsRejected.
        let mut hooks = Hooks { verified: vec![REAL.1], delivered: 0, stored: vec![] };
        let mut call = CallPresence::new();
        // The callee answered first: its OFFER arrives before this side judged anyone.
        assert!(call.on_record(&presence::offer(), &mut hooks).is_empty());
        // Its REVEAL proves the real identity, then its Accept arrives.
        assert!(call.identified(REAL.0, REAL.1, &mut hooks).is_empty(), "nothing before the answer (RD6)");
        let sent = call.answered(&mut hooks);
        assert_eq!(sent.len(), 2, "OFFER then LEASE: the peer offered first");
        assert!(presence::is_offer(&sent[0]) && Lease::decode(&sent[1]).is_some());
        assert!(call.on_record(&lease().encode(), &mut hooks).is_empty());
        assert!(call.lease_stored);
        assert_eq!(hooks.stored.len(), 1);
    }

    #[test]
    fn an_unrevealed_pseudonym_gets_nothing_and_its_lease_is_rejected() {
        let mut hooks = Hooks { verified: vec![REAL.1], delivered: 0, stored: vec![] };
        let mut call = CallPresence::new();
        assert!(call.answered(&mut hooks).is_empty());
        assert!(call.on_record(&lease().encode(), &mut hooks).is_empty());
        assert!(!call.lease_stored && hooks.delivered == 0);
        // Judged on a throwaway (never a contact): ineligible, nothing sent or stored.
        assert!(call.identified(THROWAWAY.0, THROWAWAY.1, &mut hooks).is_empty());
        assert!(call.on_record(&presence::offer(), &mut hooks).is_empty());
        assert!(call.on_record(&lease().encode(), &mut hooks).is_empty());
        assert!(hooks.stored.is_empty() && hooks.delivered == 0);
    }

    #[test]
    fn a_contact_call_offers_at_the_answer_and_stores_after_both_offers() {
        let mut hooks = Hooks { verified: vec![REAL.1], delivered: 0, stored: vec![] };
        let mut call = CallPresence::new();
        assert!(call.identified(REAL.0, REAL.1, &mut hooks).is_empty());
        let first = call.answered(&mut hooks);
        assert_eq!(first.len(), 1);
        assert!(presence::is_offer(&first[0]));
        // A lease before the peer's OFFER is refused.
        assert!(call.on_record(&lease().encode(), &mut hooks).is_empty());
        assert!(hooks.stored.is_empty());
        let reply = call.on_record(&presence::offer(), &mut hooks);
        assert_eq!(reply.len(), 1);
        assert!(Lease::decode(&reply[0]).is_some());
        // The earlier lease was rejected as "without both offers": a duplicate is too.
        assert!(call.log.iter().any(|l| l.contains("lease without both offers")));
    }
}
