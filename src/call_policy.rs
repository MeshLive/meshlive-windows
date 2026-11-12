//! What the call code may disclose, and to whom (WP4/WP6): "Who sees my
//! name", the block list and the verified contacts, in memory, with a
//! generation that moves on every real change. A call checks anything it
//! still holds or has queued (a name, a held media start, a presence record)
//! against the newest state before it leaves, and latches the blocked end
//! for a known peer that became blocked. The app's contact layer keeps it
//! current; the call code only reads it and never touches the disk for it.
//!
//! Until the app sets them: the audience is unknown and reads as Nobody (no
//! name), and both lists are unknown. An unknown contact list makes nobody a
//! verified contact. An unknown block list blocks nobody: the owner's
//! CF-CALL-24 rule (fail open; traced by the Bluetooth owner at each call
//! start; an open design question).
use crate::label_policy::NamesAudience;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub generation: u64,
    /// None until the app applied the saved choice (reads as Nobody).
    pub audience: Option<NamesAudience>,
    /// Full fingerprints; None: not loaded or unreadable.
    pub blocked: Option<Vec<[u8; 32]>>,
    pub contacts: Option<Vec<[u8; 32]>>,
}
impl Snapshot {
    pub fn audience(&self) -> NamesAudience {
        self.audience.unwrap_or(NamesAudience::Nobody)
    }
    pub fn is_blocked(&self, fingerprint: &[u8; 32]) -> bool {
        self.blocked.as_ref().is_some_and(|list| list.contains(fingerprint))
    }
    pub fn is_contact(&self, fingerprint: &[u8; 32]) -> bool {
        self.contacts.as_ref().is_some_and(|list| list.contains(fingerprint))
    }
    pub fn block_list_known(&self) -> bool {
        self.blocked.is_some()
    }
    /// Whether this PC's name may go to `peer` (its authenticated full
    /// fingerprint, None while it is an unidentified stranger) now: never to a
    /// blocked person or under Nobody; under "Only my contacts" only to an
    /// identified peer in the loaded contact list.
    pub fn name_allowed(&self, peer: Option<&[u8; 32]>) -> bool {
        if peer.is_some_and(|fingerprint| self.is_blocked(fingerprint)) {
            return false;
        }
        match self.audience() {
            NamesAudience::Nobody => false,
            NamesAudience::Everyone => true,
            NamesAudience::VerifiedOnly => peer.is_some_and(|fingerprint| self.is_contact(fingerprint)),
        }
    }
}

#[derive(Debug, Default)]
pub struct CallPolicy {
    state: Mutex<Snapshot>,
}
impl CallPolicy {
    pub fn new(snapshot: Snapshot) -> Self {
        Self { state: Mutex::new(snapshot) }
    }
    /// The app's one policy (every call session's default).
    pub fn global() -> Arc<CallPolicy> {
        static GLOBAL: OnceLock<Arc<CallPolicy>> = OnceLock::new();
        GLOBAL.get_or_init(|| Arc::new(CallPolicy::default())).clone()
    }
    pub fn snapshot(&self) -> Snapshot {
        self.lock().clone()
    }
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }
    pub fn set_audience(&self, audience: NamesAudience) {
        self.update(|s| s.audience = Some(audience));
    }
    pub fn set_blocked(&self, blocked: Option<Vec<[u8; 32]>>) {
        self.update(|s| s.blocked = blocked);
    }
    pub fn set_contacts(&self, contacts: Option<Vec<[u8; 32]>>) {
        self.update(|s| s.contacts = contacts);
    }
    fn update(&self, change: impl FnOnce(&mut Snapshot)) {
        let mut state = self.lock();
        let before = state.clone();
        change(&mut state);
        if *state != before {
            state.generation = before.generation + 1;
        }
    }
    fn lock(&self) -> MutexGuard<'_, Snapshot> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use NamesAudience::*;
    const A: [u8; 32] = [0xa1; 32];
    const B: [u8; 32] = [0xb2; 32];

    #[test]
    fn until_the_app_sets_it_no_name_goes_out_and_nobody_counts_as_a_contact() {
        let s = CallPolicy::default().snapshot();
        assert_eq!(s.audience(), Nobody);
        assert!(!s.name_allowed(None) && !s.name_allowed(Some(&A)));
        assert!(!s.is_contact(&A));
        // CF-CALL-24 (design decision, open question): an unknown block list blocks nobody.
        assert!(!s.is_blocked(&A) && !s.block_list_known());
    }

    #[test]
    fn the_name_follows_the_audience_and_a_block_wins() {
        let policy = CallPolicy::default();
        policy.set_blocked(Some(vec![B]));
        policy.set_contacts(Some(vec![A, B]));
        policy.set_audience(Everyone);
        let s = policy.snapshot();
        assert!(s.name_allowed(None), "a stranger not yet identified");
        assert!(s.name_allowed(Some(&A)));
        assert!(!s.name_allowed(Some(&B)), "blocked, even as a contact");
        policy.set_audience(VerifiedOnly);
        let s = policy.snapshot();
        assert!(s.name_allowed(Some(&A)));
        assert!(!s.name_allowed(None), "only an identified contact");
        assert!(!s.name_allowed(Some(&[0xc3; 32])));
        policy.set_contacts(None);
        assert!(!policy.snapshot().name_allowed(Some(&A)), "an unreadable contact list names nobody");
        policy.set_contacts(Some(vec![A]));
        policy.set_audience(Nobody);
        assert!(!policy.snapshot().name_allowed(Some(&A)));
    }

    #[test]
    fn every_real_change_moves_the_generation_and_a_repeat_does_not() {
        let policy = CallPolicy::default();
        let g0 = policy.generation();
        policy.set_audience(Everyone);
        let g1 = policy.generation();
        assert!(g1 > g0);
        policy.set_audience(Everyone);
        assert_eq!(policy.generation(), g1, "the same value: no change");
        policy.set_blocked(Some(vec![A]));
        assert!(policy.generation() > g1);
    }
}
