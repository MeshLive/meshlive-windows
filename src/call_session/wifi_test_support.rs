//! Test support: a `CallSession` pair with `BWF1` negotiated, for the W2 tests.
#![cfg(test)]

use super::*;
use crate::{
    bmh2::Purpose,
    framing::HopReader,
    records::{Layer, Policy, Sealer},
    session::tests::{handshake, identities},
};

pub(crate) const ID: [u8; 16] = [7; 16];

pub(crate) fn header() -> VideoHeader {
    VideoHeader {
        width: 426,
        height: 240,
        fps: 15,
        codec: 2,
        quarter_turns: 0,
        opus: true,
    }
}

pub(crate) fn packet(flags: u8, pts: i64, len: usize, byte: u8) -> Vec<u8> {
    media::Packet {
        flags,
        pts_us: pts,
        payload: &vec![byte; len],
    }
    .encode()
    .unwrap()
}

/// The caller and callee of a call whose link readers accept `BWF1`, with `UW` enabled on both.
pub(crate) fn wifi_session_pair_with<D: UnitDecoder>(
    make: fn(&[u8]) -> Result<D, opus::Error>,
    voice: bool,
    fragments: bool,
) -> (CallSession<D>, CallSession<D>, Vec<Notice>) {
    wifi_session_pair_flags(make, voice, fragments, false, false)
}

/// As `wifi_session_pair_with`, with the relay-edge flag of each side.
pub(crate) fn wifi_session_pair_flags<D: UnitDecoder>(
    make: fn(&[u8]) -> Result<D, opus::Error>,
    voice: bool,
    fragments: bool,
    caller_relay_edge: bool,
    callee_relay_edge: bool,
) -> (CallSession<D>, CallSession<D>, Vec<Notice>) {
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    let policy = Policy {
        wifi: true,
        ..Policy::default()
    };
    let mut caller_link = CallLink::continue_link_with(
        Sealer::new(Layer::Hop, al.send_key(), policy).unwrap(),
        HopReader::new(al.receive_key(), policy).unwrap(),
        ae.send_key(),
        ae.receive_key(),
        fragments,
        false,
        false,
    )
    .unwrap();
    let mut callee_link = CallLink::continue_link_with(
        Sealer::new(Layer::Hop, bl.send_key(), policy).unwrap(),
        HopReader::new(bl.receive_key(), policy).unwrap(),
        be.send_key(),
        be.receive_key(),
        fragments,
        false,
        false,
    )
    .unwrap();
    caller_link
        .enable_wifi(&[9; 32], true, caller_relay_edge)
        .unwrap();
    callee_link
        .enable_wifi(&[9; 32], false, callee_relay_edge)
        .unwrap();
    let mut caller = CallSession::outgoing(caller_link, ID, voice, 0, header(), make, 0).unwrap();
    let mut wire = Vec::new();
    while let Some(record) = caller.next_write(0).unwrap() {
        wire.extend(record);
    }
    let mut input = wire.as_slice();
    let Some(Incoming::Heartbeat(None)) = callee_link.read(&mut input).unwrap() else {
        panic!("first record was not a heartbeat");
    };
    let Some(Incoming::Signal(invite)) = callee_link.read(&mut input).unwrap() else {
        panic!("second record was not the invite");
    };
    let (mut callee, mut notices) =
        CallSession::incoming(callee_link, &invite, header(), make, 0).unwrap();
    notices.extend(callee.on_input(&mut input, 0).unwrap());
    (caller, callee, notices)
}

/// A caller whose link reader accepts `BWF1` (it signed the `WIFI` marker)
/// and a callee that never read the marker: its reader refuses type 16.
pub(crate) fn caller_only_wifi_pair<D: UnitDecoder>(
    make: fn(&[u8]) -> Result<D, opus::Error>,
    voice: bool,
) -> (CallSession<D>, CallSession<D>, Vec<Notice>) {
    let (a, b, _) = identities();
    let (al, bl) = handshake(&a, &b, Purpose::Link);
    let (ae, be) = handshake(&a, &b, Purpose::Endpoint);
    let wifi = Policy {
        wifi: true,
        ..Policy::default()
    };
    let mut caller_link = CallLink::continue_link_with(
        Sealer::new(Layer::Hop, al.send_key(), wifi).unwrap(),
        HopReader::new(al.receive_key(), wifi).unwrap(),
        ae.send_key(),
        ae.receive_key(),
        false,
        false,
        false,
    )
    .unwrap();
    let mut callee_link = CallLink::continue_link_with(
        Sealer::new(Layer::Hop, bl.send_key(), Policy::default()).unwrap(),
        HopReader::new(bl.receive_key(), Policy::default()).unwrap(),
        be.send_key(),
        be.receive_key(),
        false,
        false,
        false,
    )
    .unwrap();
    caller_link.enable_wifi(&[9; 32], true, false).unwrap();
    let mut caller = CallSession::outgoing(caller_link, ID, voice, 0, header(), make, 0).unwrap();
    let mut wire = Vec::new();
    while let Some(record) = caller.next_write(0).unwrap() {
        wire.extend(record);
    }
    let mut input = wire.as_slice();
    let Some(Incoming::Heartbeat(None)) = callee_link.read(&mut input).unwrap() else {
        panic!("first record was not a heartbeat");
    };
    let Some(Incoming::Signal(invite)) = callee_link.read(&mut input).unwrap() else {
        panic!("second record was not the invite");
    };
    let (mut callee, mut notices) =
        CallSession::incoming(callee_link, &invite, header(), make, 0).unwrap();
    notices.extend(callee.on_input(&mut input, 0).unwrap());
    (caller, callee, notices)
}
