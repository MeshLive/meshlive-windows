//! Consuming states: a failed/replayed transition cannot be retried, and a
//! responder is checked before the initiator builds/signs its identity block.
use super::{
    Error, Order, Purpose,
    guard::Guards,
    keys::{self, ApplicationKeys, HandshakeKeys},
    wire::{self, BLOCK_BYTES, Extensions},
};
use crate::{
    bmh2::{BuildTag, Capabilities, Role, SigningIdentity},
    crypto::{self, Ephemeral},
};
use subtle::{Choice, ConstantTimeEq};
use zeroize::Zeroizing;

struct Presentation<'a> {
    signer: &'a dyn SigningIdentity,
    spki: [u8; 91],
    capabilities: Capabilities,
    pseudonym: bool,
}
impl<'a> Presentation<'a> {
    fn new(
        signer: &'a dyn SigningIdentity,
        capabilities: Capabilities,
        pseudonym: bool,
    ) -> Result<Self, Error> {
        let spki = signer.public_key()?;
        crypto::validate_agreement_public(&spki)?;
        Ok(Self {
            signer,
            spki,
            capabilities,
            pseudonym,
        })
    }
}
pub struct Config<'a> {
    purpose: Purpose,
    real: Presentation<'a>,
    stranger: Option<Presentation<'a>>,
    build_tag: Option<BuildTag>,
}
impl<'a> Config<'a> {
    pub fn new(
        purpose: Purpose,
        identity: &'a dyn SigningIdentity,
        capabilities: Capabilities,
    ) -> Result<Self, Error> {
        Ok(Self {
            purpose,
            real: Presentation::new(identity, capabilities, false)?,
            stranger: None,
            build_tag: None,
        })
    }
    /// The identity owner supplies the protocol's per-attempt/per-epoch throwaway
    /// key. This module never creates a persistent identity or reuses one as it.
    pub fn with_stranger(
        mut self,
        identity: &'a dyn SigningIdentity,
        capabilities: Capabilities,
    ) -> Result<Self, Error> {
        let stranger = Presentation::new(identity, capabilities, true)?;
        if stranger.spki == self.real.spki {
            return Err(Error::Policy);
        }
        self.stranger = Some(stranger);
        Ok(self)
    }
    pub fn with_build_tag(mut self, tag: BuildTag) -> Result<Self, Error> {
        if self.purpose == Purpose::Link {
            if !(1..=32767).contains(&tag.release) {
                return Err(Error::Format);
            }
            self.build_tag = Some(tag);
        }
        Ok(self)
    }
}
pub struct Peer {
    spki: [u8; 91],
    node_id: [u8; 16],
    fingerprint: [u8; 32],
    capabilities: Capabilities,
    extensions: Extensions,
}
impl Peer {
    pub fn public_key(&self) -> &[u8; 91] {
        &self.spki
    }
    pub fn node_id(&self) -> &[u8; 16] {
        &self.node_id
    }
    pub fn fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }
    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }
}
pub struct Session {
    keys: ApplicationKeys,
    peer: Peer,
    order: Order,
    tag_matched: bool,
    own_pseudonym: bool,
    own_spki: [u8; 91],
}
impl Session {
    pub fn keys(&self) -> &ApplicationKeys {
        &self.keys
    }
    pub fn peer(&self) -> &Peer {
        &self.peer
    }
    pub fn order(&self) -> Order {
        self.order
    }
    pub fn tag_matched(&self) -> bool {
        self.tag_matched
    }
    pub fn own_pseudonym(&self) -> bool {
        self.own_pseudonym
    }
    pub fn own_public_key(&self) -> &[u8; 91] {
        &self.own_spki
    }
}

/// Presence-tag data prepared by the lease/advert owner. Verification-code
/// reservation, replay pins and verify-only scoping are not implemented here.
pub struct ContactTag {
    key: Zeroizing<[u8; 32]>,
    binding: [u8; 11],
    expected_peer: [u8; 16],
}
impl ContactTag {
    pub fn new(key: [u8; 32], binding: [u8; 11], expected_peer: [u8; 16]) -> Self {
        Self {
            key: Zeroizing::new(key),
            binding,
            expected_peer,
        }
    }
}
pub struct TagCheck {
    key: Zeroizing<[u8; 32]>,
    binding: [u8; 11],
}
impl TagCheck {
    pub fn new(key: [u8; 32], binding: [u8; 11]) -> Self {
        Self {
            key: Zeroizing::new(key),
            binding,
        }
    }
}
pub fn contact_tag(
    purpose: Purpose,
    key: &[u8; 32],
    zero_tag_message: &[u8; 122],
    binding: &[u8; 11],
) -> Result<[u8; 16], Error> {
    if zero_tag_message[106..].iter().any(|b| *b != 0) {
        return Err(Error::Format);
    }
    let hash = keys::transcript_hash(purpose, zero_tag_message)?;
    let mac = crypto::hmac_sha256(key, &[hash.as_slice(), binding].concat())?;
    Ok(mac[..16].try_into().unwrap())
}

struct Party<'a> {
    config: Config<'a>,
    role: Role,
    stranger: bool,
    point: [u8; 65],
    ephemeral: Option<Ephemeral>,
    transcript: Zeroizing<Vec<u8>>,
    keys: Option<HandshakeKeys>,
    #[cfg(test)]
    entropy: Option<TestEntropy>,
}
impl<'a> Party<'a> {
    fn new(config: Config<'a>, role: Role) -> Self {
        Self {
            config,
            role,
            stranger: false,
            point: [0; 65],
            ephemeral: None,
            transcript: Zeroizing::new(Vec::with_capacity(900)),
            keys: None,
            #[cfg(test)]
            entropy: None,
        }
    }
    fn presented(&self) -> &Presentation<'a> {
        if self.stranger {
            self.config.stranger.as_ref().unwrap()
        } else {
            &self.config.real
        }
    }
    fn draw(&mut self) -> Result<[u8; 32], Error> {
        #[cfg(test)]
        if let Some(entropy) = &mut self.entropy {
            let key = entropy.ephemeral.take().ok_or(Error::State)?;
            self.point.copy_from_slice(&key.public_key()?[26..]);
            self.ephemeral = Some(key);
            return Ok(entropy.nonce);
        }
        let key = Ephemeral::generate()?;
        self.point.copy_from_slice(&key.public_key()?[26..]);
        self.ephemeral = Some(key);
        Ok(crypto::random_nonce()?)
    }
    fn random_tag(&self) -> Result<[u8; 16], Error> {
        #[cfg(test)]
        if let Some(entropy) = &self.entropy {
            return Ok(entropy.tag);
        }
        Ok(crypto::random_nonce()?[..16].try_into().unwrap())
    }
    fn derive(&mut self, peer: &[u8; 65]) -> Result<(), Error> {
        self.keys = Some(HandshakeKeys::derive(
            self.config.purpose,
            self.role,
            self.ephemeral.take().ok_or(Error::State)?,
            peer,
            &self.transcript,
        )?);
        Ok(())
    }
    fn key(&mut self) -> Result<&mut HandshakeKeys, Error> {
        self.keys.as_mut().ok_or(Error::State)
    }
    fn make_block(
        &self,
        carrier: &[u8],
        mut extensions: Extensions,
    ) -> Result<Zeroizing<[u8; BLOCK_BYTES]>, Error> {
        let presented = self.presented();
        let mut caps = presented.capabilities;
        if self.config.purpose == Purpose::Link {
            caps.transport &= 0x7f;
        }
        if presented.pseudonym {
            extensions.opening = None;
            if self.config.purpose == Purpose::Link {
                caps.transport &= !8;
            }
        }
        extensions.pseudonym = presented.pseudonym;
        extensions.build_tag = self.config.build_tag;
        let mut block = Zeroizing::new([0; BLOCK_BYTES]);
        block[..2].copy_from_slice(&91_u16.to_be_bytes());
        block[2..93].copy_from_slice(&presented.spki);
        block[93..100].copy_from_slice(&caps.encode());
        block[100..102].copy_from_slice(&112_u16.to_be_bytes());
        block[102..214].copy_from_slice(&extensions.encode()?);
        let hash = keys::transcript_hash(
            self.config.purpose,
            &[&self.transcript[..], carrier, &block[..214]].concat(),
        )?;
        let signed = keys::signature_context(self.config.purpose, self.role, &hash);
        let raw = crypto::der_to_raw(&presented.signer.sign(&signed)?)?;
        crypto::verify_p256_raw(&presented.spki, &signed, &raw)?;
        block[214..278].copy_from_slice(&raw);
        let hash = keys::transcript_hash(
            self.config.purpose,
            &[&self.transcript[..], carrier, &block[..278]].concat(),
        )?;
        let mac = self
            .keys
            .as_ref()
            .ok_or(Error::State)?
            .finished_mac(true, &hash)?;
        block[278..].copy_from_slice(&mac[..]);
        Ok(block)
    }
    fn verify_block(
        &self,
        carrier: &[u8],
        block: &[u8; BLOCK_BYTES],
        role: Role,
        number: u8,
        order: Order,
    ) -> Result<Peer, Error> {
        if block[..2] != 91_u16.to_be_bytes() || block[100..102] != 112_u16.to_be_bytes() {
            return Err(Error::Format);
        }
        let spki: [u8; 91] = block[2..93].try_into().unwrap();
        let mut capabilities = Capabilities::decode(&block[93..100]).map_err(|_| Error::Format)?;
        let hash = keys::transcript_hash(
            self.config.purpose,
            &[&self.transcript[..], carrier, &block[..214]].concat(),
        )?;
        crypto::verify_p256_raw(
            &spki,
            &keys::signature_context(self.config.purpose, role, &hash),
            block[214..278].try_into().unwrap(),
        )
        .map_err(|_| Error::Signature)?;
        let hash = keys::transcript_hash(
            self.config.purpose,
            &[&self.transcript[..], carrier, &block[..278]].concat(),
        )?;
        let mac = self
            .keys
            .as_ref()
            .ok_or(Error::State)?
            .finished_mac(false, &hash)?;
        if !bool::from(mac[..].ct_eq(&block[278..])) {
            return Err(Error::Mac);
        }
        let extensions = Extensions::parse(&block[102..214])?;
        extensions.check_placement(self.config.purpose, role, number, order)?;
        if spki == self.config.real.spki
            || self
                .config
                .stranger
                .as_ref()
                .is_some_and(|s| spki == s.spki)
        {
            return Err(Error::Policy);
        }
        let fingerprint = crypto::sha256(&spki)?;
        let node_id = fingerprint[..16].try_into().unwrap();
        if self.config.purpose == Purpose::Link {
            capabilities.transport &= 0x7f;
        }
        Ok(Peer {
            spki,
            node_id,
            fingerprint,
            capabilities,
            extensions,
        })
    }
    fn finish(mut self, peer: Peer, order: Order, tag_matched: bool) -> Result<Session, Error> {
        let presented = self.presented();
        let own_spki = presented.spki;
        let own_pseudonym = presented.pseudonym;
        let keys = self
            .keys
            .take()
            .ok_or(Error::State)?
            .application(&self.transcript)?;
        Ok(Session {
            keys,
            peer,
            order,
            tag_matched,
            own_pseudonym,
            own_spki,
        })
    }
}

#[derive(Default)]
pub struct InitiatorOptions {
    /// Must reflect explicit local user consent. Default callers never reveal first.
    pub may_reveal_first: bool,
    pub tag: Option<ContactTag>,
    pub intent: Option<u8>,
}
pub struct Waiting2<'a> {
    party: Party<'a>,
    options: InitiatorOptions,
}
pub enum InitiatorProgress<'a> {
    Complete(Session),
    Waiting4(Waiting4<'a>),
}
pub struct Waiting4<'a> {
    party: Party<'a>,
}
impl<'a> Waiting2<'a> {
    pub fn start(config: Config<'a>, options: InitiatorOptions) -> Result<(Self, Vec<u8>), Error> {
        Self::begin(Party::new(config, Role::Initiator), options)
    }
    fn begin(mut party: Party<'a>, options: InitiatorOptions) -> Result<(Self, Vec<u8>), Error> {
        party.stranger = options.tag.is_none() && party.config.stranger.is_some();
        let nonce = party.draw()?;
        let mut message = Vec::with_capacity(122);
        message.extend_from_slice(&wire::header(
            party.config.purpose,
            1,
            u8::from(options.may_reveal_first),
        )?);
        message.push(1);
        message.extend_from_slice(&party.point);
        message.extend_from_slice(&nonce);
        let tag = if let Some(tag) = &options.tag {
            let zero: [u8; 122] = [message.as_slice(), &[0; 16]].concat().try_into().unwrap();
            contact_tag(party.config.purpose, &tag.key, &zero, &tag.binding)?
        } else {
            party.random_tag()?
        };
        message.extend_from_slice(&tag);
        party.transcript.extend_from_slice(&message);
        Ok((Self { party, options }, message))
    }
    /// `check_responder` must enforce advert/RDI binding and verified contacts.
    /// It runs before any caller identity is signed or returned in R_FIRST.
    pub fn receive(
        mut self,
        message: &[u8],
        check_responder: impl FnOnce(&Peer) -> Result<(), Error>,
    ) -> Result<(InitiatorProgress<'a>, Vec<u8>), Error> {
        let m = wire::Message2::parse(message, self.party.config.purpose)?;
        let order = if m.responder_first {
            Order::ResponderFirst
        } else {
            Order::InitiatorFirst
        };
        if !m.responder_first && (self.options.tag.is_some() || !self.options.may_reveal_first) {
            return Err(Error::Policy);
        }
        self.party.transcript.extend_from_slice(m.clear);
        self.party.derive(m.point)?;
        let mut peer = None;
        if let Some(sealed) = m.sealed {
            let block = self.party.key()?.open(m.clear, sealed)?;
            let checked = self
                .party
                .verify_block(&[], &block, Role::Responder, 2, order)?;
            if self
                .options
                .tag
                .as_ref()
                .is_some_and(|t| t.expected_peer != checked.node_id)
            {
                return Err(Error::Policy);
            }
            check_responder(&checked)?;
            if checked.extensions.pseudonym && !self.party.presented().pseudonym {
                return Err(Error::Policy);
            }
            self.party.transcript.extend_from_slice(&block[..]);
            peer = Some(checked);
        }
        let h3 = wire::header(self.party.config.purpose, 3, 0)?;
        let mut extensions = Extensions::default();
        if order == Order::InitiatorFirst {
            extensions.intent = self.options.intent;
        }
        let block = self.party.make_block(&h3, extensions)?;
        let message = [h3.as_slice(), &self.party.key()?.seal(&h3, &block)?].concat();
        self.party.transcript.extend_from_slice(&h3);
        self.party.transcript.extend_from_slice(&block[..]);
        let progress = if let Some(peer) = peer {
            InitiatorProgress::Complete(self.party.finish(peer, order, false)?)
        } else {
            InitiatorProgress::Waiting4(Waiting4 { party: self.party })
        };
        Ok((progress, message))
    }
}
impl<'a> Waiting4<'a> {
    /// I_FIRST already disclosed locally; the same responder checks still run
    /// before session keys can be returned to the caller.
    pub fn receive(
        mut self,
        message: &[u8],
        check_responder: impl FnOnce(&Peer) -> Result<(), Error>,
    ) -> Result<Session, Error> {
        let (header, sealed) = wire::parse_encrypted(message, self.party.config.purpose, 4)?;
        let block = self.party.key()?.open(header, sealed)?;
        let peer =
            self.party
                .verify_block(header, &block, Role::Responder, 4, Order::InitiatorFirst)?;
        check_responder(&peer)?;
        self.party.transcript.extend_from_slice(header);
        self.party.transcript.extend_from_slice(&block[..]);
        self.party.finish(peer, Order::InitiatorFirst, false)
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum ResponderMode {
    #[default]
    Everyone,
    ContactsOnly,
}
#[derive(Default)]
pub struct ResponderOptions<'a> {
    pub mode: ResponderMode,
    pub tags: &'a [TagCheck],
    pub opening: Option<Vec<u8>>,
    /// Required with pseudonyms: the discovery owner's validated current epoch.
    pub discovery_epoch: Option<u32>,
}
pub struct Waiting3<'a> {
    party: Party<'a>,
    order: Order,
    tag_matched: bool,
    opening: Option<Vec<u8>>,
}
impl<'a> Waiting3<'a> {
    /// Reuse `guards` across all attempts. Runs on the protocol worker, with a
    /// bounded signing-budget wait before drawing fresh entropy or signing.
    pub fn respond(
        config: Config<'a>,
        message: &[u8],
        options: ResponderOptions<'_>,
        guards: &mut Guards,
    ) -> Result<(Self, Vec<u8>), Error> {
        Self::begin(Party::new(config, Role::Responder), message, options, guards)
    }
    /// `respond` with a gate that runs when a tag matched, with the index of
    /// the matching `options.tags` entry and msg1's ephemeral point, before
    /// any key is drawn or anything is signed (BVMID2 single-use reservation:
    /// PROTOCOL "Displayer and single use"). An error refuses the attempt.
    pub fn respond_gated(
        config: Config<'a>,
        message: &[u8],
        options: ResponderOptions<'_>,
        guards: &mut Guards,
        gate: &mut dyn FnMut(usize, &[u8; 65]) -> Result<(), Error>,
    ) -> Result<(Self, Vec<u8>), Error> {
        Self::begin_gated(Party::new(config, Role::Responder), message, options, guards, gate)
    }
    fn begin(
        party: Party<'a>,
        message: &[u8],
        options: ResponderOptions<'_>,
        guards: &mut Guards,
    ) -> Result<(Self, Vec<u8>), Error> {
        Self::begin_gated(party, message, options, guards, &mut |_, _| Ok(()))
    }
    fn begin_gated(
        mut party: Party<'a>,
        message: &[u8],
        options: ResponderOptions<'_>,
        guards: &mut Guards,
        gate: &mut dyn FnMut(usize, &[u8; 65]) -> Result<(), Error>,
    ) -> Result<(Self, Vec<u8>), Error> {
        let m = wire::Message1::parse(message, party.config.purpose)?;
        let mut guards =
            guards.for_attempt(party.config.stranger.is_some(), options.discovery_epoch)?;
        guards.first_use(m.point)?;
        let mut zero = [0; 122];
        zero[..106].copy_from_slice(&message[..106]);
        let mut matched = Choice::from(0);
        let mut index = None;
        for (i, tag) in options.tags.iter().enumerate() {
            let expected = contact_tag(party.config.purpose, &tag.key, &zero, &tag.binding)?;
            let this = expected.as_slice().ct_eq(m.tag.as_slice());
            matched |= this;
            if index.is_none() && bool::from(this) {
                index = Some(i);
            }
        }
        let tag_matched = bool::from(matched);
        if let Some(i) = index {
            gate(i, m.point)?;
        }
        let order = if options.mode == ResponderMode::Everyone || tag_matched {
            Order::ResponderFirst
        } else {
            Order::InitiatorFirst
        };
        if order == Order::InitiatorFirst && !m.may_reveal_first {
            return Err(Error::Policy);
        }
        if order == Order::ResponderFirst && !tag_matched {
            guards.spend()?;
            party.stranger = party.config.stranger.is_some();
        }
        let nonce = party.draw()?;
        if &party.point == m.point {
            return Err(Error::Format);
        }
        let mut clear = wire::header(
            party.config.purpose,
            2,
            u8::from(order == Order::ResponderFirst),
        )?
        .to_vec();
        clear.extend_from_slice(&party.point);
        clear.extend_from_slice(&nonce);
        party.transcript.extend_from_slice(message);
        party.transcript.extend_from_slice(&clear);
        party.derive(m.point)?;
        let mut output = clear.clone();
        if order == Order::ResponderFirst {
            let mut extensions = Extensions::default();
            extensions.opening = options.opening.clone();
            let block = party.make_block(&[], extensions)?;
            output.extend_from_slice(&party.key()?.seal(&clear, &block)?);
            party.transcript.extend_from_slice(&block[..]);
        }
        Ok((
            Self {
                party,
                order,
                tag_matched,
                opening: options.opening,
            },
            output,
        ))
    }
    /// `admit` must enforce current lease recipients, contact/pseudonym policy
    /// and local admission. Refusal withholds msg4 and every session key.
    pub fn receive(
        mut self,
        message: &[u8],
        admit: impl FnOnce(&Peer, bool) -> Result<(), Error>,
    ) -> Result<(Session, Option<Vec<u8>>), Error> {
        let (header, sealed) = wire::parse_encrypted(message, self.party.config.purpose, 3)?;
        let block = self.party.key()?.open(header, sealed)?;
        let peer = self
            .party
            .verify_block(header, &block, Role::Initiator, 3, self.order)?;
        // Contacts-only I_FIRST answers an admitted identity. A declared
        // throwaway must never be promoted to one by an owner callback.
        if self.order == Order::InitiatorFirst && peer.extensions.pseudonym {
            return Err(Error::Policy);
        }
        admit(&peer, self.tag_matched)?;
        self.party.transcript.extend_from_slice(header);
        self.party.transcript.extend_from_slice(&block[..]);
        let mut message4 = None;
        if self.order == Order::InitiatorFirst {
            let h4 = wire::header(self.party.config.purpose, 4, 0)?;
            let mut extensions = Extensions::default();
            extensions.opening = self.opening;
            let block = self.party.make_block(&h4, extensions)?;
            message4 = Some([h4.as_slice(), &self.party.key()?.seal(&h4, &block)?].concat());
            self.party.transcript.extend_from_slice(&h4);
            self.party.transcript.extend_from_slice(&block[..]);
        }
        Ok((
            self.party.finish(peer, self.order, self.tag_matched)?,
            message4,
        ))
    }
}

#[cfg(test)]
struct TestEntropy {
    ephemeral: Option<Ephemeral>,
    nonce: [u8; 32],
    tag: [u8; 16],
}
#[cfg(test)]
mod tests;
