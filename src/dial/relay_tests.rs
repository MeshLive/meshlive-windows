use super::*;
use crate::{bmh3::{guard::Guards, handshake::{Waiting3, ResponderOptions, Session}}, discovery::{Announcement, GATT, RELAY}, session::tests::identities};
const EPOCH: u32 = 1_990_656;

fn handshake(purpose: Purpose, caps: Capabilities, opening: Option<Vec<u8>>, check: impl FnOnce(&Peer) -> Result<(), bmh3::Error>) -> Result<(Session, Session), bmh3::Error> {
    let (answerer, caller, _) = identities();
    let throwaway = Identity::ephemeral().unwrap();
    let config = stranger_config(purpose, &caller, &throwaway, Capabilities { transport: 4, ..caps })?;
    let (waiting, m1) = Waiting2::start(config, InitiatorOptions::default())?;
    let (answer, m2) = Waiting3::respond(Config::new(purpose, &answerer, caps)?, &m1,
        ResponderOptions { opening, ..Default::default() }, &mut Guards::default())?;
    let (InitiatorProgress::Complete(client), m3) = waiting.receive(&m2, check)? else { return Err(bmh3::Error::Policy) };
    let (server, m4) = answer.receive(&m3, |_, tagged| { assert!(!tagged, "first-hop relay receives no contact tag"); Ok(()) })?;
    assert!(m4.is_none());
    assert!(server.peer().extensions().pseudonym);
    assert_ne!(server.peer().public_key(), &caller.public_key().unwrap(), "first-hop relay must not learn the caller's real link identity");
    Ok((client, server))
}
fn caps(transport: u8, children: u8) -> Capabilities {
    Capabilities { transport, encode_codecs: 3, decode_codecs: 3, maximum_bitrate_kbps: 1100, maximum_children: children }
}

#[test]
fn relay_first_hop_requires_the_signed_gatt_call_relay_role_before_sending_identity() {
    crate::identity_gate::set_store_dir(None);
    for (transport, children, accepted) in [(0x44, 1, true), (4, 1, false), (0x44, 0, false), (0x4c, 1, false), (0x41, 1, false)] {
        assert_eq!(handshake(Purpose::Link, caps(transport, children), None, relay_peer).is_ok(), accepted);
    }
}

#[test]
fn relayed_endpoint_must_open_the_callee_rdi_and_match_a_selected_full_contact() {
    crate::identity_gate::set_store_dir(None);
    let (callee, _, node) = identities();
    let entry = rotating::OpeningEntry::own(&[3; 32], &node, EPOCH, rotating::ROLE_CALL_OFFER).unwrap();
    let opening = rotating::encode_body(&[entry.clone()]).unwrap();
    let contact = ContactDial { contact: node, lease_key: [7; 32] };
    let (endpoint, _) = handshake(Purpose::Endpoint, caps(0x0c, 0), Some(opening.clone()), |peer| selected_peer(peer, &entry.rdi, true, EPOCH, Some(&contact))).unwrap();
    assert_eq!(endpoint.peer().node_id(), &node);
    let link_key = Identity::ephemeral().unwrap().public_key().unwrap();
    assert!(endpoint_matches_link(endpoint.peer(), &link_key, 1));
    assert!(!endpoint_matches_link(endpoint.peer(), &callee.public_key().unwrap(), 1), "a relay cannot become the call endpoint");
    let mut wrong_rdi = entry.rdi; wrong_rdi[0] ^= 1;
    assert!(handshake(Purpose::Endpoint, caps(0x0c, 0), Some(opening.clone()), |peer| selected_peer(peer, &wrong_rdi, true, EPOCH, None)).is_err());
    let wrong_contact = ContactDial { contact: [9; 16], lease_key: [7; 32] };
    assert!(handshake(Purpose::Endpoint, caps(0x0c, 0), Some(opening), |peer| selected_peer(peer, &entry.rdi, true, EPOCH, Some(&wrong_contact))).is_err());
}

#[test]
fn only_rotating_gatt_call_mirrors_inside_the_android_chain_bound_are_offered() {
    let mut offer = Announcement { version: 5, prefix: [1; 6], flags: CALL | GATT | RELAY, summary: 3, token: 0, cost: 1,
        sequence: 0, width: 0, height: 0, fps: 0, bitrate: 0, recovery: None };
    for cost in 1..=5 { offer.cost = cost; assert!(!call_offer(&offer, false)); assert_eq!(call_offer(&offer, true), cost <= 4); }
    offer.cost = 1; offer.version = 2; offer.summary |= BMH3_HINT;
    assert!(!call_offer(&offer, true), "no legacy or BMH2 relay fallback in this arm");
    offer.version = 5; offer.flags |= crate::discovery::STREAM;
    assert!(!call_offer(&offer, true));
    offer.flags = CALL | RELAY;
    assert!(!call_offer(&offer, true));
}
