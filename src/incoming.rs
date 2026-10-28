//! The callee side of a call over a link a phone opened to this PC (Design
//! QW: possible only once the PC announces itself). Android's responder
//! rules: BMH3 at both layers in R_FIRST ("Everyone"); this PC presents its
//! per-epoch throwaway to an untagged stranger and its real key only to a
//! tagged initiator (rule P): a contact whose lease tag matched one of this
//! PC's on-air call IDs, or the scanner of the BVMID2 code this PC shows.
//! A lease-tag match must come from a contact this PC delivered a lease to;
//! a code-tag match runs the single-use reservation (`qr_display`) at msg1
//! of both layers and commits at the completed end-to-end handshake. Real
//! keys pass the identity gate. BDO1 is exchanged (own pinned entries) when
//! both link hellos carry bit 3: this PC's throwaway link block clears it.
use crate::{
    bmh2::{Capabilities, SigningIdentity},
    bmh3::{
        self, Purpose,
        guard::Guards,
        handshake::{Config, Peer, ResponderMode, ResponderOptions, TagCheck, Waiting3},
    },
    call::CallLink,
    dial::{Error, Stage},
    framing::{HandshakeReader, HopReader, frame_handshake},
    qr_display::{Display, Gate},
    records::{Layer, Policy, Sealer},
    rotating::{self, OpeningEntry},
    verify_scan::Carrier,
};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

fn fail(stage: Stage, detail: &str) -> Error {
    Error {
        stage,
        detail: detail.into(),
    }
}
fn at<E: std::fmt::Debug>(stage: Stage) -> impl Fn(E) -> Error {
    move |e| Error {
        stage,
        detail: format!("{e:?}"),
    }
}

pub const CAPABILITIES: Capabilities = Capabilities {
    transport: 0x0c,
    encode_codecs: 0x03,
    decode_codecs: 0x03,
    maximum_bitrate_kbps: 1100,
    maximum_children: 0,
};
/// What this PC's throwaway signs in a LINK block as the responder: link-hello
/// bit 3 cleared, so no `BDO1` (and no salt) follows (Android
/// `Bmh3Presentation.stranger`). End-to-end blocks keep bit 3 (`BDX1` REVEAL).
pub const STRANGER_LINK_CAPABILITIES: Capabilities = Capabilities {
    transport: CAPABILITIES.transport & !rotating::LINK_OPENING_BIT,
    ..CAPABILITIES
};
/// Android's callee deadline for both handshakes.
pub const HANDSHAKE_SECONDS: u64 = 20;

/// Who a tag belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagOwner {
    /// A contact's lease (its node ID): only that contact may present it.
    Contact([u8; 16]),
    /// The BVMID2 code this PC shows.
    Code,
    /// This PC's own lease tag for the RDI on air. Every contact holding
    /// the current lease shares the day key, so the tag alone cannot name
    /// the contact: the authenticated caller must be one this PC delivered
    /// the current lease to.
    Leased,
}

pub struct Listener<'a> {
    pub real: &'a dyn SigningIdentity,
    /// This epoch's throwaway (stranger presentation).
    pub pseudonym: &'a dyn SigningIdentity,
    /// Tag checks (lease tags for each on-air call ID, the code's) and,
    /// at the same index, who each belongs to.
    pub tags: Vec<TagCheck>,
    pub owners: Vec<TagOwner>,
    /// This PC's pinned role-0 opening entries (BMH3 opening and BDO1).
    pub entries: Vec<OpeningEntry>,
    /// The validated current epoch (pseudonym guards).
    pub epoch: u32,
}

/// An authenticated incoming link, ready for the caller's Invite.
pub struct Answered {
    pub link: CallLink,
    pub peer: [u8; 91],
    pub peer_node: [u8; 16],
    pub peer_fingerprint: [u8; 32],
    pub peer_pseudonymous: bool,
    pub own_pseudonymous: bool,
    pub tag: Option<TagOwner>,
    /// Both endpoint hellos carried bit 3.
    pub negotiated: bool,
    /// The caller's endpoint hello decode codecs (this PC's video header).
    pub peer_decode_codecs: u8,
    /// The caller's endpoint hello capabilities.
    pub peer_capabilities: Capabilities,
    /// The end-to-end peer is not the authenticated link peer (a relayed
    /// call); false on a direct link.
    pub relayed: bool,
    /// The end-to-end exporter (REVEAL binding).
    pub exporter: [u8; 32],
    pub buffer: Vec<u8>,
}

fn pull<C: Carrier>(carrier: &mut C, buffer: &mut Vec<u8>, stage: Stage, deadline: Instant, cancelled: &AtomicBool) -> Result<(), Error> {
    if Instant::now() >= deadline {
        return Err(fail(stage, "timeout"));
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(fail(stage, "cancelled"));
    }
    carrier.receive(buffer, Duration::from_millis(50)).map(|_| ()).map_err(|e| fail(stage, &e))
}

/// Both responder handshakes. `display`/`gate`: the shown code's single-use
/// reservation; `delivered(node)`: this PC gave that contact a lease.
#[allow(clippy::too_many_arguments)]
pub fn respond<C: Carrier>(
    carrier: &mut C,
    listener: &Listener,
    guards: &mut Guards,
    mut display: Option<(&mut Display, Gate)>,
    delivered: &dyn Fn(&[u8; 16]) -> bool,
    cancelled: &AtomicBool,
    trace: &mut dyn FnMut(&str, &str),
) -> Result<Answered, Error> {
    let deadline = Instant::now() + Duration::from_secs(HANDSHAKE_SECONDS);
    let checks = &listener.tags;
    if listener.owners.len() != checks.len() {
        return Err(fail(Stage::Link, "tag owners"));
    }
    let opening = Some(rotating::encode_body(&listener.entries).map_err(at(Stage::Link))?);
    // Which tag matched, per layer (set by the gate before any key is drawn).
    let mut matched: Option<TagOwner> = None;

    let mut buffer = Vec::new();
    let mut frames = HandshakeReader::default();
    let m1 = loop {
        let mut input = buffer.as_slice();
        let got = frames.read(&mut input).map_err(at(Stage::Link))?;
        let used = buffer.len() - input.len();
        buffer.drain(..used);
        if let Some(m) = got {
            break m;
        }
        pull(carrier, &mut buffer, Stage::Link, deadline, cancelled)?;
    };
    let config = Config::new(Purpose::Link, listener.real, crate::video::hello_codecs(CAPABILITIES, false))
        .map_err(at(Stage::Link))?
        .with_stranger(listener.pseudonym, crate::video::hello_codecs(STRANGER_LINK_CAPABILITIES, false))
        .map_err(at(Stage::Link))?;
    let options = ResponderOptions {
        mode: ResponderMode::Everyone,
        tags: checks,
        opening: opening.clone(),
        discovery_epoch: Some(listener.epoch),
    };
    let (waiting, m2) = {
        let mut gate = |index: usize, point: &[u8; 65]| -> Result<(), bmh3::Error> {
            let owner = listener.owners[index];
            if owner == TagOwner::Code {
                let (d, g) = display.as_mut().ok_or(bmh3::Error::Policy)?;
                d.on_tag_match(*g, Purpose::Link, point).map_err(|_| bmh3::Error::Policy)?;
            }
            matched = Some(owner);
            Ok(())
        };
        Waiting3::respond_gated(config, &m1, options, guards, &mut gate).map_err(at(Stage::Link))?
    };
    carrier
        .send(&frame_handshake(&m2).map_err(at(Stage::Link))?, deadline)
        .map_err(|e| fail(Stage::Link, &e))?;
    let m3 = loop {
        let mut input = buffer.as_slice();
        let got = frames.read(&mut input).map_err(at(Stage::Link))?;
        let used = buffer.len() - input.len();
        buffer.drain(..used);
        if let Some(m) = got {
            break m;
        }
        pull(carrier, &mut buffer, Stage::Link, deadline, cancelled)?;
    };
    let link_tag = matched;
    let admit = |peer: &Peer, tag_matched: bool, tag: Option<TagOwner>| -> Result<(), bmh3::Error> {
        if tag_matched {
            // A tagged initiator must present the tag owner's real key.
            if peer.extensions().pseudonym {
                return Err(bmh3::Error::Policy);
            }
            if let Some(TagOwner::Contact(node)) = tag {
                if peer.node_id() != &node || !delivered(&node) {
                    return Err(bmh3::Error::Policy);
                }
            }
            if tag == Some(TagOwner::Leased) && !delivered(peer.node_id()) {
                return Err(bmh3::Error::Policy);
            }
        }
        if !peer.extensions().pseudonym {
            crate::identity_gate::admit(peer.node_id(), peer.fingerprint()).map_err(|_| bmh3::Error::Policy)?;
        }
        Ok(())
    };
    let (link, m4) = waiting
        .receive(&m3, |peer, tag_matched| admit(peer, tag_matched, link_tag))
        .map_err(at(Stage::Link))?;
    if let Some(m4) = m4 {
        carrier
            .send(&frame_handshake(&m4).map_err(at(Stage::Link))?, deadline)
            .map_err(|e| fail(Stage::Link, &e))?;
    }
    let link_peer = *link.peer().public_key();
    // The responder skips only when it presented its throwaway (whose block
    // cleared bit 3); its real key answering any initiator exchanges.
    let own_hello = if link.own_pseudonym() { STRANGER_LINK_CAPABILITIES } else { CAPABILITIES };
    let opening_exchange = rotating::link_opening_exchanged(
        false,
        own_hello.transport,
        link.peer().capabilities().transport,
        link.own_pseudonym(),
        link.peer().extensions().pseudonym,
    );
    trace("callee_link_done", &format!("tagged={} bdo1={opening_exchange}", link_tag.is_some()));
    let policy = Policy {
        require_opening: opening_exchange,
        ..Policy::default()
    };
    let mut hop_out = Sealer::new(Layer::Hop, link.keys().send_key(), policy).map_err(at(Stage::Link))?;
    let mut hop_in = HopReader::new(link.keys().receive_key(), policy).map_err(at(Stage::Link))?;
    let read_hop = |carrier: &mut C, buffer: &mut Vec<u8>, hop_in: &mut HopReader, stage: Stage| loop {
        let mut input = buffer.as_slice();
        let record = hop_in.read(&mut input, 4096).map_err(at(stage))?;
        let used = buffer.len() - input.len();
        buffer.drain(..used);
        if let Some(r) = record {
            return Ok::<_, Error>(r);
        }
        pull(carrier, buffer, stage, deadline, cancelled)?;
    };
    if opening_exchange {
        let own = rotating::encode_bdo1(&listener.entries).map_err(at(Stage::Link))?;
        carrier
            .send(&hop_out.seal(13, 0, &own).map_err(at(Stage::Link))?, deadline)
            .map_err(|e| fail(Stage::Link, &e))?;
        let record = read_hop(carrier, &mut buffer, &mut hop_in, Stage::Link)?;
        if record.header.kind != 13 {
            return Err(fail(Stage::Link, "expected the discovery opening"));
        }
        rotating::decode_bdo1(&record.plaintext).map_err(at(Stage::Link))?;
    }

    // End-to-end layer in BMS1 type-4 records.
    let record = read_hop(carrier, &mut buffer, &mut hop_in, Stage::Endpoint)?;
    if record.header.kind != 4 {
        return Err(fail(Stage::Endpoint, "unexpected hop record type"));
    }
    // The endpoint hello offers call video fragments (bit 7, CF-MEDIA-76).
    let endpoint_caps = crate::video::hello_codecs(Capabilities {
        transport: crate::call::fragments::endpoint_transport(CAPABILITIES.transport) | if crate::labels::enabled() { crate::labels::CAPABILITY_BIT } else { 0 },
        // The decoders this PC really has (CF-HS-02), not a constant; Design
        // HC's masks (with bit 4) through `hello_codecs`.
        decode_codecs: crate::video::decode_mask(),
        ..CAPABILITIES
    }, true);
    let config = Config::new(Purpose::Endpoint, listener.real, endpoint_caps)
        .map_err(at(Stage::Endpoint))?
        .with_stranger(listener.pseudonym, endpoint_caps)
        .map_err(at(Stage::Endpoint))?;
    let options = ResponderOptions {
        mode: ResponderMode::Everyone,
        tags: checks,
        opening,
        discovery_epoch: Some(listener.epoch),
    };
    let mut endpoint_tag = None;
    let (waiting, m2) = {
        let mut gate = |index: usize, point: &[u8; 65]| -> Result<(), bmh3::Error> {
            let owner = listener.owners[index];
            if owner == TagOwner::Code {
                let (d, g) = display.as_mut().ok_or(bmh3::Error::Policy)?;
                d.on_tag_match(*g, Purpose::Endpoint, point).map_err(|_| bmh3::Error::Policy)?;
            }
            endpoint_tag = Some(owner);
            Ok(())
        };
        Waiting3::respond_gated(config, &record.plaintext, options, guards, &mut gate).map_err(at(Stage::Endpoint))?
    };
    // The same initiator at both layers: same tag owner.
    if endpoint_tag != link_tag {
        return Err(fail(Stage::Endpoint, "tag changed between layers"));
    }
    carrier
        .send(&hop_out.seal(4, 0, &m2).map_err(at(Stage::Endpoint))?, deadline)
        .map_err(|e| fail(Stage::Endpoint, &e))?;
    let record = read_hop(carrier, &mut buffer, &mut hop_in, Stage::Endpoint)?;
    if record.header.kind != 4 {
        return Err(fail(Stage::Endpoint, "unexpected hop record type"));
    }
    let (endpoint, m4) = waiting
        .receive(&record.plaintext, |peer, tag_matched| {
            // A direct link: the same key end to end (CF-DISC-21).
            if !peer.extensions().pseudonym && *peer.public_key() != link_peer {
                return Err(bmh3::Error::Policy);
            }
            admit(peer, tag_matched, endpoint_tag)
        })
        .map_err(at(Stage::Endpoint))?;
    if let Some(m4) = m4 {
        carrier
            .send(&hop_out.seal(4, 0, &m4).map_err(at(Stage::Endpoint))?, deadline)
            .map_err(|e| fail(Stage::Endpoint, &e))?;
    }
    let peer = endpoint.peer();
    if link_tag == Some(TagOwner::Code) {
        if let Some((d, g)) = display.as_mut() {
            if !d.complete(*g, *peer.fingerprint()) {
                return Err(fail(Stage::Endpoint, "code no longer reserved"));
            }
        }
    }
    let negotiated = peer.capabilities().transport & 8 != 0 && CAPABILITIES.transport & 8 != 0;
    trace(
        "callee_endpoint_done",
        &format!("pseudonymous_peer={} own_pseudonym={} negotiated={negotiated}", peer.extensions().pseudonym, endpoint.own_pseudonym()),
    );
    let answered = Answered {
        peer: *peer.public_key(),
        peer_node: *peer.node_id(),
        peer_fingerprint: *peer.fingerprint(),
        peer_pseudonymous: peer.extensions().pseudonym,
        own_pseudonymous: endpoint.own_pseudonym(),
        tag: link_tag,
        negotiated,
        peer_decode_codecs: peer.capabilities().decode_codecs,
        peer_capabilities: peer.capabilities(),
        relayed: *peer.public_key() != link_peer,
        exporter: *endpoint.keys().exporter(),
        link: CallLink::continue_link_with(
            hop_out,
            hop_in,
            endpoint.keys().send_key(),
            endpoint.keys().receive_key(),
            crate::call::fragments::negotiated(crate::call::fragments::local_enabled(), peer.capabilities().transport, 0),
            negotiated,
            crate::labels::enabled() && peer.capabilities().transport & crate::labels::CAPABILITY_BIT != 0,
        )
            .map_err(at(Stage::Call))?,
        buffer,
    };
    Ok(answered)
}

#[cfg(test)]
mod tests;
