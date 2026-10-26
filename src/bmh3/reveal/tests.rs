use super::*;
use crate::{
    bmh3::tests::{bytes, scalar, vectors},
    crypto::tests::FixtureIdentity,
};
use serde_json::Value;

fn vector() -> Value {
    vectors()["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "endpoint_pseudonyms")
        .unwrap()
        .clone()
}
struct Fixture {
    v: Value,
    exporter: [u8; 32],
    ps_i: [u8; 91],
    ps_r: [u8; 91],
    real_i: [u8; 91],
    real_r: [u8; 91],
    entry: OpeningEntry,
}
impl Fixture {
    fn new() -> Self {
        let v = vector();
        let r = &v["reveal"];
        let key = |field| -> [u8; 91] { bytes(r, field).try_into().unwrap() };
        let input = |field| -> [u8; 91] { bytes(&v["inputs"], field).try_into().unwrap() };
        // The callee's pinned entry is the PROTOCOL rotating-ID role-0 vector.
        let node: [u8; 16] = crate::bmh3::tests::hex("a1b2c3d4e5f60718293a4b5c6d7e8f90")
            .try_into()
            .unwrap();
        Self {
            exporter: bytes(r, "exporter").try_into().unwrap(),
            ps_i: input("identity_I_spki"),
            ps_r: input("identity_R_spki"),
            real_i: key("real_I_spki"),
            real_r: key("real_R_spki"),
            entry: OpeningEntry::own(&std::array::from_fn(|i| i as u8), &node, 1_990_656, 0)
                .unwrap(),
            v,
        }
    }
    fn r(&self, field: &str) -> Vec<u8> {
        bytes(&self.v["reveal"], field)
    }
    fn hash(&self) -> [u8; 32] {
        self.r("callee_record_hash").try_into().unwrap()
    }
    fn identity(&self, role: u8) -> FixtureIdentity {
        let suffix = if role == ROLE_CALLER { "I" } else { "R" };
        FixtureIdentity::new(
            &self.r(&format!("real_{suffix}_spki")),
            &scalar(&self.v["reveal"], &format!("real_{suffix}_scalar")),
        )
    }
    fn verify_callee(&self, record: &[u8]) -> Result<Verified, Error> {
        verify(
            &Reveal::decode(record)?,
            ROLE_CALLEE,
            &self.exporter,
            &self.ps_r,
            &self.ps_i,
            &[self.ps_i, self.real_i],
            None,
        )
    }
    fn verify_caller(&self, record: &[u8], prior: &[u8; 32]) -> Result<Verified, Error> {
        verify(
            &Reveal::decode(record)?,
            ROLE_CALLER,
            &self.exporter,
            &self.ps_i,
            &self.ps_r,
            &[self.ps_r, self.real_r],
            Some(prior),
        )
    }
}

#[test]
fn android_reveal_vectors_match_inputs_records_hash_and_verify_both_roles() {
    let f = Fixture::new();
    // The reveal key comes from this session's exporter (the BMH3 trace value).
    assert_eq!(
        f.exporter.as_slice(),
        bytes(&f.v["initiator_trace"], "exporter")
    );
    let callee_input = signing_input(
        ROLE_CALLEE,
        &f.exporter,
        &f.ps_r,
        &f.ps_i,
        &f.real_r,
        std::slice::from_ref(&f.entry),
        None,
    )
    .unwrap();
    assert_eq!(callee_input, f.r("callee_input"));
    let callee = Reveal {
        role: ROLE_CALLEE,
        spki: f.real_r,
        entries: vec![f.entry.clone()],
        signature: f.r("callee_sig").try_into().unwrap(),
    };
    assert_eq!(callee.encode().unwrap(), f.r("callee"));
    assert_eq!(Reveal::decode(&f.r("callee")).unwrap(), callee);
    assert_eq!(crypto::sha256(&f.r("callee")).unwrap(), f.hash());
    let caller_input = signing_input(
        ROLE_CALLER,
        &f.exporter,
        &f.ps_i,
        &f.ps_r,
        &f.real_i,
        &[],
        Some(&f.hash()),
    )
    .unwrap();
    assert_eq!(caller_input, f.r("caller_input"));
    let caller = Reveal {
        role: ROLE_CALLER,
        spki: f.real_i,
        entries: vec![],
        signature: f.r("caller_sig").try_into().unwrap(),
    };
    assert_eq!(caller.encode().unwrap(), f.r("caller"));
    assert_eq!(Reveal::decode(&f.r("caller")).unwrap(), caller);
    let v = f.verify_callee(&f.r("callee")).unwrap();
    assert_eq!(v.node_id.as_slice(), f.r("node_id_real_R"));
    assert_eq!(v.entries, vec![f.entry.clone()]);
    let v = f.verify_caller(&f.r("caller"), &f.hash()).unwrap();
    assert_eq!(v.node_id.as_slice(), f.r("node_id_real_I"));
    // Native CNG signatures (random k) over the same inputs also verify.
    let built = build(
        ROLE_CALLEE,
        &f.identity(ROLE_CALLEE),
        &f.exporter,
        &f.ps_r,
        &f.ps_i,
        std::slice::from_ref(&f.entry),
        None,
    )
    .unwrap();
    assert_eq!(
        built.encode().unwrap()[..FIXED_BYTES + 43],
        f.r("callee")[..FIXED_BYTES + 43]
    );
    f.verify_callee(&built.encode().unwrap()).unwrap();
}

#[test]
fn android_reveal_negative_cases_and_strict_codec_fail_closed() {
    let f = Fixture::new();
    let callee = f.r("callee");
    let caller = f.r("caller");
    // 19: a reveal replayed into another session (different exporter).
    let mut other = f.exporter;
    other[0] ^= 1;
    assert_eq!(
        verify(
            &Reveal::decode(&callee).unwrap(),
            ROLE_CALLEE,
            &other,
            &f.ps_r,
            &f.ps_i,
            &[],
            None
        ),
        Err(Error::Signature)
    );
    // 20: a caller reveal relabelled as the callee's (the role is signed).
    let mut relabelled = caller.clone();
    relabelled[5] = ROLE_CALLEE;
    assert_eq!(
        verify(
            &Reveal::decode(&relabelled).unwrap(),
            ROLE_CALLEE,
            &f.exporter,
            &f.ps_i,
            &f.ps_r,
            &[],
            Some(&f.hash())
        ),
        Err(Error::Signature)
    );
    // 21: a reveal that names the pseudonym, or this phone's own key.
    for spki in [f.ps_r, f.ps_i, f.real_i] {
        let mut named = Reveal::decode(&callee).unwrap();
        named.spki = spki;
        assert_eq!(
            f.verify_callee(&named.encode().unwrap()),
            Err(Error::Policy)
        );
    }
    // 22: an altered opening entry; 23: a valid signature under another key.
    let mut altered = callee.clone();
    altered[100] ^= 1;
    assert_eq!(f.verify_callee(&altered), Err(Error::Signature));
    let mut other_key = Reveal::decode(&callee).unwrap();
    other_key.spki = f.real_i;
    assert_eq!(
        verify(
            &other_key,
            ROLE_CALLEE,
            &f.exporter,
            &f.ps_r,
            &f.ps_i,
            &[],
            None
        ),
        Err(Error::Signature)
    );
    // 26: a caller reveal that carries opening entries is refused before signing.
    let mut with_entry = Reveal::decode(&caller).unwrap();
    with_entry.entries.push(f.entry.clone());
    assert_eq!(with_entry.encode(), Err(Error::Format));
    let forged = [
        &caller[..FIXED_BYTES - 1],
        &[1],
        &callee[FIXED_BYTES..FIXED_BYTES + 43],
        &caller[FIXED_BYTES..],
    ]
    .concat();
    assert_eq!(Reveal::decode(&forged), Err(Error::Format));
    let signer = f.identity(ROLE_CALLER);
    assert_eq!(
        build(
            ROLE_CALLER,
            &signer,
            &f.exporter,
            &f.ps_i,
            &f.ps_r,
            std::slice::from_ref(&f.entry),
            Some(&f.hash())
        )
        .map(|_| ()),
        Err(Error::Format)
    );
    // 27: a caller reveal that does not bind the callee's record.
    let mut prior = f.hash();
    prior[31] ^= 1;
    assert_eq!(f.verify_caller(&caller, &prior), Err(Error::Signature));
    // Wrong expected role, every truncation, trailing byte, header and count.
    assert_eq!(
        verify(
            &Reveal::decode(&caller).unwrap(),
            ROLE_CALLEE,
            &f.exporter,
            &f.ps_i,
            &f.ps_r,
            &[],
            None
        ),
        Err(Error::Format)
    );
    for cut in 0..callee.len() {
        assert!(Reveal::decode(&callee[..cut]).is_err(), "cut {cut}");
    }
    assert!(Reveal::decode(&[callee.as_slice(), &[0]].concat()).is_err());
    for (index, value) in [
        (0, b'X'),
        (4, 1),
        (4, 5),
        (5, 0),
        (5, 3),
        (6, 1),
        (7, 90),
        (99, 0),
        (99, 2),
        (99, 3),
    ] {
        let mut changed = callee.clone();
        changed[index] = value;
        assert!(Reveal::decode(&changed).is_err(), "byte {index}");
    }
    let mut off_curve = Reveal::decode(&callee).unwrap();
    off_curve.spki[60] ^= 1;
    assert!(f.verify_callee(&off_curve.encode().unwrap()).is_err());
    for bit in 0..callee.len() * 8 {
        let mut changed = callee.clone();
        changed[bit / 8] ^= 1 << (bit % 8);
        assert!(f.verify_callee(&changed).is_err(), "bit {bit}");
    }
}

#[test]
fn reveal_sessions_enforce_callee_first_one_per_direction_and_latch_failures() {
    let f = Fixture::new();
    let (real_i, real_r) = (f.identity(ROLE_CALLER), f.identity(ROLE_CALLEE));
    let session = |role, pseudonymous| {
        let (own, peer, real) = if role == ROLE_CALLER {
            (f.ps_i, f.ps_r, f.real_i)
        } else {
            (f.ps_r, f.ps_i, f.real_r)
        };
        RevealSession::new(
            role,
            &f.exporter,
            own,
            peer,
            true,
            pseudonymous,
            vec![own, real],
        )
        .unwrap()
    };
    let mut caller = session(ROLE_CALLER, true);
    let mut callee = session(ROLE_CALLEE, true);
    assert!(caller.peer_reveal_required());
    assert!(!caller.caller_may_reveal());
    assert_eq!(caller.send_own(&real_i, &[]), Err(Error::State));
    // A caller reveal before the callee's own is a protocol error that latches.
    let early = build(
        ROLE_CALLER,
        &real_i,
        &f.exporter,
        &f.ps_i,
        &f.ps_r,
        &[],
        Some(&f.hash()),
    )
    .unwrap()
    .encode()
    .unwrap();
    let mut premature = session(ROLE_CALLEE, true);
    assert_eq!(
        premature.on_peer_reveal(&early).map(|_| ()),
        Err(Error::Policy)
    );
    assert_eq!(premature.send_own(&real_r, &[]), Err(Error::State));

    let record = callee
        .send_own(&real_r, std::slice::from_ref(&f.entry))
        .unwrap();
    assert_eq!(callee.send_own(&real_r, &[]), Err(Error::State));
    let verified = caller.on_peer_reveal(&record).unwrap().clone();
    assert_eq!(verified.spki, f.real_r);
    assert_eq!(verified.entries, vec![f.entry.clone()]);
    assert!(!caller.peer_reveal_required());
    assert!(caller.caller_may_reveal());
    assert_eq!(
        caller.on_peer_reveal(&record).map(|_| ()),
        Err(Error::Policy)
    );
    let mut caller = session(ROLE_CALLER, true);
    caller.on_peer_reveal(&record).unwrap();
    assert_eq!(
        caller.send_own(&real_i, std::slice::from_ref(&f.entry)),
        Err(Error::State)
    );
    let reply = caller.send_own(&real_i, &[]).unwrap();
    assert!(!caller.caller_may_reveal());
    assert_eq!(callee.on_peer_reveal(&reply).unwrap().spki, f.real_i);

    // A peer that did not declare a pseudonym neither sends nor receives one.
    let mut plain = session(ROLE_CALLER, false);
    assert_eq!(
        plain.on_peer_reveal(&record).map(|_| ()),
        Err(Error::Policy)
    );
    let mut plain = session(ROLE_CALLEE, false);
    assert_eq!(plain.send_own(&real_r, &[]), Err(Error::State));
    // A tampered callee reveal latches the caller's exchange.
    let mut caller = session(ROLE_CALLER, true);
    let mut bad = record.clone();
    *bad.last_mut().unwrap() ^= 1;
    assert!(caller.on_peer_reveal(&bad).is_err());
    assert_eq!(
        caller.on_peer_reveal(&record).map(|_| ()),
        Err(Error::State)
    );
    assert!(RevealSession::new(3, &f.exporter, f.ps_i, f.ps_r, true, true, vec![]).is_err());
}
