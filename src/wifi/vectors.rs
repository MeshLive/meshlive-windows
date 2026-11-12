//! Shared Android Wi-Fi vectors, consumed byte for byte.

use serde_json::Value;
use std::sync::OnceLock;

use super::{keys, roles, wire};

const VECTORS: &str = include_str!("../../tests/vectors/conformance/wifi-vectors.json");

fn root() -> &'static Value {
    static ROOT: OnceLock<Value> = OnceLock::new();
    ROOT.get_or_init(|| {
        let value: Value =
            serde_json::from_str(VECTORS).expect("wifi-vectors.json is not valid JSON");
        assert_eq!(
            value["version"].as_u64(),
            Some(1),
            "wifi-vectors.json version must be 1: regenerate against A0 or amend step 1.9"
        );
        value
    })
}

fn section(path: &str) -> &'static Value {
    let mut value = root();
    for part in path.split('.') {
        value = value.get(part).unwrap_or_else(|| {
            panic!("wifi-vectors.json section {path} is missing: regenerate against A0 or amend step 1.9")
        });
    }
    value
}

fn h(value: &Value) -> Vec<u8> {
    let text = value
        .as_str()
        .unwrap_or_else(|| panic!("wifi-vectors.json hex field is not a string: {value:?}"));
    if text.len() % 2 != 0 {
        panic!("wifi-vectors.json hex field has an odd length: {text}");
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("hex is ASCII");
            u8::from_str_radix(pair, 16)
                .unwrap_or_else(|_| panic!("wifi-vectors.json hex field is invalid: {text}"))
        })
        .collect()
}

fn array<const N: usize>(value: &Value) -> [u8; N] {
    h(value)
        .try_into()
        .unwrap_or_else(|bytes: Vec<u8>| panic!("wifi-vectors.json needs {N} bytes, got {}", bytes.len()))
}
fn number<T: TryFrom<u64>>(value: &Value) -> T
where
    T::Error: std::fmt::Debug,
{
    T::try_from(value.as_u64().expect("wifi-vectors.json number is missing"))
        .expect("wifi-vectors.json number is out of range")
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("wifi-vectors.json field {key} is missing"))
}
fn ipv4(value: &Value) -> [u8; 4] {
    let text = value.as_str().expect("ipv4 is a string");
    let mut out = [0; 4];
    for (slot, part) in out.iter_mut().zip(text.split('.')) {
        *slot = part.parse().expect("ipv4 octet");
    }
    out
}

#[test]
fn the_android_vectors_for_uw_ssid_psk_and_the_binding_keys_match() {
    let keys_value = section("keys");
    let exporter: [u8; 32] = array(&keys_value["exporter"]);
    let uw = keys::uw_for_call(&exporter).unwrap();
    assert!(*uw == array::<32>(&keys_value["uwCall"]), "mismatch");

    let ticket: [u8; 8] = array(&keys_value["ticket"]);
    let ticket2: [u8; 8] = array(&keys_value["ticket2"]);
    assert_eq!(
        keys::direct_ssid(&uw, &ticket).unwrap(),
        text(keys_value, "ssid")
    );
    assert!(
        keys::direct_psk(&uw, &ticket).unwrap().as_str() == text(keys_value, "psk"),
        "mismatch"
    );
    assert_eq!(
        keys::direct_ssid(&uw, &ticket2).unwrap(),
        text(keys_value, "ssidTicket2")
    );
    assert!(
        keys::direct_psk(&uw, &ticket2).unwrap().as_str() == text(keys_value, "pskTicket2"),
        "mismatch"
    );

    let bind = section("bind");
    let challenge: [u8; 16] = array(&bind["challenge"]);
    let challenge2: [u8; 16] = array(&bind["challenge2"]);
    let guest_point: [u8; 65] = array(&bind["guestPoint"]);
    let guest_nonce: [u8; 32] = array(&bind["guestNonce"]);
    let host_point: [u8; 65] = array(&bind["hostPoint"]);
    let host_nonce: [u8; 32] = array(&bind["hostNonce"]);

    let frame1 = wire::frame1(&challenge);
    assert!(frame1.as_slice() == h(&bind["frame1"]).as_slice(), "mismatch");
    let pre = keys::pre(&uw, &ticket, &challenge, &guest_point, &guest_nonce).unwrap();
    assert!(pre.as_slice() == h(&bind["pre"]).as_slice(), "mismatch");
    let frame2 = wire::frame2(&ticket, &guest_point, &guest_nonce, &pre);
    assert!(frame2.as_slice() == h(&bind["frame2"]).as_slice(), "mismatch");
    let frame3 = wire::frame3(&host_point, &host_nonce);
    assert!(frame3.as_slice() == h(&bind["frame3"]).as_slice(), "mismatch");

    let second = keys::pre(&uw, &ticket, &challenge2, &guest_point, &guest_nonce).unwrap();
    assert!(
        second.as_slice() == h(&bind["preWithSecondChallenge"]).as_slice(),
        "mismatch"
    );
    assert!(second != pre, "mismatch");

    let th = keys::transcript_hash(&frame1, &frame2, &frame3).unwrap();
    assert!(th.as_slice() == h(&bind["th"]).as_slice(), "mismatch");
    let salt = keys::bind_salt(&uw, &ticket, &th).unwrap();
    assert!(salt.as_slice() == h(&bind["salt"]).as_slice(), "mismatch");
    let dh: [u8; 32] = array(&bind["dh"]);
    let prk = keys::bind_prk(&salt, &dh).unwrap();
    assert!(prk.as_slice() == h(&bind["K"]).as_slice(), "mismatch");
    let bind_keys = keys::bind_keys(&uw, &ticket, &th, &dh).unwrap();
    assert!(bind_keys.g2h.as_slice() == h(&bind["kG2H"]).as_slice(), "mismatch");
    assert!(bind_keys.h2g.as_slice() == h(&bind["kH2G"]).as_slice(), "mismatch");
}

#[test]
fn the_android_bwf1_records_decode_and_re_encode_to_the_same_bytes() {
    let records = section("bwf1").as_array().expect("bwf1 is an array");
    for entry in records {
        let name = text(entry, "name");
        let bytes = h(&entry["hex"]);
        let record = wire::Bwf1::decode(&bytes)
            .unwrap_or_else(|error| panic!("{name}: {}", error.name()));
        assert_eq!(record.kind(), number::<u8>(&entry["kind"]), "{name}");
        let decoded = &entry["decoded"];
        match record {
            wire::Bwf1::Caps(value) => {
                assert_eq!(value.bearers, number::<u8>(&decoded["bearers"]), "{name}");
                assert_eq!(value.free_host, number::<u8>(&decoded["freeHost"]), "{name}");
                assert_eq!(value.free_guest, number::<u8>(&decoded["freeGuest"]), "{name}");
            }
            wire::Bwf1::Host(value) => {
                assert_eq!(value.status, number::<u8>(&decoded["status"]), "{name}");
                assert_eq!(value.bearer, number::<u8>(&decoded["bearer"]), "{name}");
                assert!(value.ticket.as_slice() == h(&decoded["ticket"]).as_slice(), "{name}");
                assert_eq!(value.valid_ms, number::<u16>(&decoded["validMs"]), "{name}");
                assert_eq!(value.freq_mhz, number::<u16>(&decoded["freqMhz"]), "{name}");
            }
            wire::Bwf1::Decline(value) => {
                assert!(value.ticket.as_slice() == h(&decoded["ticket"]).as_slice(), "{name}");
                assert_eq!(value.reason, number::<u8>(&decoded["reason"]), "{name}");
            }
            wire::Bwf1::Pick(value) => {
                assert!(value.ticket.as_slice() == h(&decoded["ticket"]).as_slice(), "{name}");
                assert!(
                    value.challenge.as_slice() == h(&decoded["challenge"]).as_slice(),
                    "{name}"
                );
            }
            wire::Bwf1::Armed(value) => {
                assert!(value.ticket.as_slice() == h(&decoded["ticket"]).as_slice(), "{name}");
            }
            wire::Bwf1::Bind(value) => {
                assert_eq!(value.role, number::<u8>(&decoded["role"]), "{name}");
                assert!(value.ticket.as_slice() == h(&decoded["ticket"]).as_slice(), "{name}");
            }
            wire::Bwf1::Ready(value) => {
                assert_eq!(
                    value.recv_ceiling_kbps,
                    number::<u16>(&decoded["recvCeilingKbps"]),
                    "{name}"
                );
                assert_eq!(
                    value.family_cap_kbps,
                    number::<u16>(&decoded["familyCapKbps"]),
                    "{name}"
                );
                assert_eq!(
                    value.transport_budget_kbps,
                    number::<u16>(&decoded["transportBudgetKbps"]),
                    "{name}"
                );
            }
            wire::Bwf1::Switch(value) => {
                assert_eq!(value.generation, number::<u32>(&decoded["gen"]), "{name}");
                assert!(value.ticket.as_slice() == h(&decoded["ticket"]).as_slice(), "{name}");
                assert_eq!(value.e2e_boundary, number::<u64>(&decoded["e2eBoundary"]), "{name}");
                assert_eq!(value.mesh_boundary, number::<u64>(&decoded["meshBoundary"]), "{name}");
            }
            wire::Bwf1::Resume(value) => {
                assert_eq!(value.generation, number::<u32>(&decoded["gen"]), "{name}");
                assert!(value.ticket.as_slice() == h(&decoded["ticket"]).as_slice(), "{name}");
                assert_eq!(value.e2e_next, number::<u64>(&decoded["e2eNext"]), "{name}");
                assert_eq!(value.mesh_next, number::<u64>(&decoded["meshNext"]), "{name}");
            }
            wire::Bwf1::Close(value) => {
                assert_eq!(value.reason, number::<u8>(&decoded["reason"]), "{name}");
            }
            wire::Bwf1::Creds(value) => {
                assert!(value.ticket.as_slice() == h(&decoded["ticket"]).as_slice(), "{name}");
                assert_eq!(value.port, number::<u16>(&decoded["port"]), "{name}");
                assert!(value.ipv4 == ipv4(&decoded["ipv4"]), "{name}");
            }
        }
        // Every valid record says whether it re-encodes byte for byte; one that carried ignored bits must not (X5).
        let round_trip = entry["roundTrip"]
            .as_bool()
            .unwrap_or_else(|| panic!("{name}: roundTrip is missing"));
        if round_trip {
            assert!(record.encode().unwrap() == bytes, "{name}");
        } else {
            assert!(record.encode().unwrap() != bytes, "{name}: ignored bits must be written as 0");
        }
    }
}

#[test]
fn the_android_malformed_records_and_frames_fail_with_the_named_error() {
    for entry in section("bwf1Invalid")
        .as_array()
        .expect("bwf1Invalid is an array")
    {
        let name = text(entry, "name");
        let bytes = h(&entry["hex"]);
        let error = wire::Bwf1::decode(&bytes)
            .err()
            .unwrap_or_else(|| panic!("{name}: unexpectedly decoded"));
        assert_eq!(error.name(), text(entry, "error"), "{name}");
    }
    for entry in section("wireInvalid")
        .as_array()
        .expect("wireInvalid is an array")
    {
        let frame = number::<u8>(&entry["frame"]);
        if frame == 0 {
            continue;
        }
        let name = text(entry, "name");
        let bytes = h(&entry["hex"]);
        let error = match frame {
            1 => wire::parse_frame1(&bytes).map(|_| ()),
            2 => wire::parse_frame2(&bytes).map(|_| ()),
            3 => wire::parse_frame3(&bytes).map(|_| ()),
            _ => panic!("{name}: unknown BWU1 frame {frame}"),
        }
        .err()
        .unwrap_or_else(|| panic!("{name}: unexpectedly parsed"));
        assert_eq!(error.name(), text(entry, "error"), "{name}");
    }
}

#[test]
fn the_android_role_table_is_reproduced_row_for_row() {
    for row in section("roles.rows")
        .as_array()
        .expect("roles.rows is an array")
    {
        let values: Vec<u8> = row
            .as_array()
            .expect("roles row is an array")
            .iter()
            .map(|value| number::<u8>(value))
            .collect();
        assert_eq!(values.len(), 9, "roles row width");
        let local = wire::Caps {
            bearers: values[0],
            free_host: values[1],
            free_guest: values[2],
        };
        let peer = wire::Caps {
            bearers: values[3],
            free_host: values[4],
            free_guest: values[5],
        };
        let chosen = roles::choose(&local, &peer, values[6] == 1);
        if values[7] == 0 {
            assert!(chosen.is_none(), "row {values:?}");
        } else {
            let chosen = chosen.unwrap_or_else(|| panic!("row {values:?}"));
            assert_eq!(chosen.bearer, values[7], "row {values:?}");
            assert_eq!(chosen.local == roles::Role::Host, values[8] == 1, "row {values:?}");
        }
    }
}

#[test]
fn the_android_off_curve_point_is_refused_and_the_good_point_is_accepted() {
    let bind = section("bind");
    let good: [u8; 65] = array(&bind["guestPoint"]);
    let good_spki = keys::spki_of(&good).expect("good point has the uncompressed prefix");
    assert!(crate::crypto::validate_agreement_public(&good_spki).is_ok());
    let off_curve: [u8; 65] = array(&bind["offCurveGuestPoint"]);
    let off_curve_spki = keys::spki_of(&off_curve).expect("off-curve point has the uncompressed prefix");
    assert!(crate::crypto::validate_agreement_public(&off_curve_spki).is_err());
}
