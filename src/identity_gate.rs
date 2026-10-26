//! Peer identity gate (CF-HS-50; Android `PeerIdentityGate`): every
//! end-to-end peer's real key is remembered (last 1,024 node IDs) and
//! compared with the verified contacts. A verified contact with the same
//! 48-bit prefix but another key is a MISMATCH ("possible impersonation"),
//! refused before this PC presents its real key or uses a content key
//! (policy REFUSE, Android's default). An unreadable store is fail-open
//! (CF-HS-51): nobody is refused because of it. Pseudonym keys are never
//! admitted; a pseudonym's real key is admitted at its REVEAL.
use crate::{
    contacts::{ContactStore, Fingerprint},
    presence::PeerState,
};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

pub const MAX_RECENT: usize = 1024;

#[derive(Default)]
struct Gate {
    store_dir: Option<PathBuf>,
    recent: VecDeque<([u8; 16], Fingerprint)>,
}
fn gate() -> &'static Mutex<Gate> {
    static GATE: OnceLock<Mutex<Gate>> = OnceLock::new();
    GATE.get_or_init(Default::default)
}

/// The folder holding the verified-contact store; None: no store attached
/// (every peer is admitted, as on Android without a store).
pub fn set_store_dir(dir: Option<PathBuf>) {
    gate().lock().unwrap_or_else(|e| e.into_inner()).store_dir = dir;
}

/// The verified-contact state of a peer (Android `VerifiedContactStore.state`).
pub fn state(store: &mut ContactStore, node: &[u8; 16], fingerprint: &Fingerprint) -> PeerState {
    if fingerprint[..16] != node[..] {
        return PeerState::Mismatch;
    }
    let Ok(list) = store.list() else {
        return PeerState::StoreUnreadable;
    };
    if list.iter().any(|c| &c.fingerprint == fingerprint) {
        PeerState::Verified
    } else if list.iter().any(|c| c.fingerprint[..6] == fingerprint[..6]) {
        PeerState::Mismatch
    } else {
        PeerState::Unverified
    }
}

/// The peer's last authenticated fingerprint, if it was seen recently.
pub fn authenticated_fingerprint(node: &[u8; 16]) -> Option<Fingerprint> {
    let g = gate().lock().unwrap_or_else(|e| e.into_inner());
    g.recent.iter().rev().find(|(n, _)| n == node).map(|(_, f)| *f)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mismatch;

/// An authenticated forwarder grants only the original source's full node ID.
pub fn admit_source(node: &[u8; 16]) -> Result<(), Mismatch> {
    let dir = gate().lock().unwrap_or_else(|e| e.into_inner()).store_dir.clone();
    let Some(dir) = dir else { return Ok(()) };
    let Ok(list) = ContactStore::open_default(&dir).list() else { return Ok(()) };
    if list.iter().any(|c| c.node_id()[..6] == node[..6] && c.node_id() != *node) { Err(Mismatch) } else { Ok(()) }
}

/// Admits an authenticated real key (fingerprint = SHA-256 of its SPKI).
/// Err for a MISMATCH under the REFUSE policy.
pub fn admit(node: &[u8; 16], fingerprint: &Fingerprint) -> Result<PeerState, Mismatch> {
    let dir = {
        let mut g = gate().lock().unwrap_or_else(|e| e.into_inner());
        g.recent.retain(|(n, _)| n != node);
        g.recent.push_back((*node, *fingerprint));
        while g.recent.len() > MAX_RECENT {
            g.recent.pop_front();
        }
        g.store_dir.clone()
    };
    let state = match dir {
        None if fingerprint[..16] != node[..] => PeerState::Mismatch,
        None => PeerState::Unverified,
        Some(dir) => state(&mut ContactStore::open_default(&dir), node, fingerprint),
    };
    match state {
        PeerState::Mismatch => {
            eprintln!(
                "SECURITY IDENTITY MISMATCH: peer node prefix {} matches a verified contact but the key differs; possible impersonation | policy=REFUSE",
                node[..6].iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
            Err(Mismatch)
        }
        PeerState::StoreUnreadable => {
            eprintln!("SECURITY verified-contact store unreadable; peer not compared (fail-open)");
            Ok(state)
        }
        _ => Ok(state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{contacts::Method, sealed::tests::{Plain, scratch}};

    #[test]
    fn a_ground_prefix_collision_with_a_verified_contact_is_refused_and_unknown_peers_pass() {
        let dir = scratch("identity-gate");
        let mut store = ContactStore::new(&dir, Box::new(Plain));
        let asha: Fingerprint = [0xa5; 32];
        assert_eq!(store.mark_verified(Some(&asha), &asha, "Asha", Method::QrScan, 1), crate::contacts::Verify::Verified);
        let node = |f: &Fingerprint| -> [u8; 16] { f[..16].try_into().unwrap() };
        assert_eq!(state(&mut store, &node(&asha), &asha), PeerState::Verified);
        let mut forged = asha;
        forged[31] ^= 1; // same 48-bit prefix (and node ID), another key
        assert_eq!(state(&mut store, &node(&forged), &forged), PeerState::Mismatch);
        let mut prefix_only = [0x11; 32];
        prefix_only[..6].copy_from_slice(&asha[..6]);
        assert_eq!(state(&mut store, &node(&prefix_only), &prefix_only), PeerState::Mismatch);
        let stranger = [0x22; 32];
        assert_eq!(state(&mut store, &node(&stranger), &stranger), PeerState::Unverified);
        assert_eq!(state(&mut store, &[0x33; 16], &stranger), PeerState::Mismatch, "node ID not of this key");
        // The process gate over that store.
        set_store_dir(Some(dir.clone()));
        // Store files are sealed with the test sealer here, so the gate's DPAPI
        // read fails: fail-open, nobody refused (CF-HS-51).
        assert_eq!(admit(&node(&forged), &forged), Ok(PeerState::StoreUnreadable));
        set_store_dir(None);
        assert_eq!(admit(&node(&stranger), &stranger), Ok(PeerState::Unverified));
        assert_eq!(authenticated_fingerprint(&node(&stranger)), Some(stranger));
        assert_eq!(admit(&[0x33; 16], &stranger), Err(Mismatch));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
