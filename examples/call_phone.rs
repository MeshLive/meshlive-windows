//! First real interop test: this PC calls an Android phone (run only with
//! `--approved` on test devices you own; it scans, connects and talks
//! on the real radio). It dials the version 5 (rotating ID) call offer whose
//! advertised 6 bytes equal `--rdi` (never a node prefix), speaks BMH3 as a
//! stranger (fresh throwaway key at link and end to end, Phase 2 rules),
//! places a voice call (Camera off) with a quiet generated Opus tone, verifies
//! the callee REVEAL against the selected RDI before its Accept counts, reveals
//! a fresh non-persisted test identity in return, and hangs up after
//! `--seconds` of media. No camera, microphone or audio output is opened.
//!
//! With `--video` it places a video call instead: after Accept (StartMedia
//! with the camera on, as Android) a generated moving test pattern is encoded
//! once at Android's call defaults (426x240, 15 FPS, 200 kbps) with the codec
//! the call header announced, alongside the tone. Still no camera.
//!
//! `--video-backend hardware|libaom` restricts the encoder (default: Android's
//! order). The phone's video is decoded with Media Foundation in a video call
//! (counted, not shown) and discarded in a voice call, so the receive queue
//! never fills. A `stats` line is logged every second.
//!
//! With `--contact <fingerprint hex or name>` it calls a stored verified
//! contact instead (no `--rdi`): the first version 5 call offer whose rotating
//! ID the presence leases recognise as that contact is dialled with the
//! contact tag and this PC's persistent identity (`dial::dial_contact`). Stores:
//! the app's own unless `--data-dir`.
//!
//! A version 2 offer without the BMH3 hint (a default iPhone/Mac) is dialled
//! with BMH2 and this PC's persistent identity.
//!
//! `--persistent` (with `--rdi`) reveals this PC's persistent identity after
//! the callee's REVEAL instead of a fresh test identity. With it or with
//! `--contact`, Phase 2 presence leases are exchanged after the answer,
//! judged on the peer's real (revealed) identity, using the app's stores.
//!
//! Timings (UTC ms on every line): `scan_found` (discovery), `ringing`
//! (dial start to the callee's Ringing) and `active` (Ringing to media).
//!
//! `--camera` / `--mic` send the first camera's pictures and the
//! default microphone instead of the generated pattern and tone; nothing is
//! recorded or kept.
//!
//! `--measure-scan N` first samples the WinRT watcher N s as configured and N s
//! with every report (Experimental knob), one stack_fact line each.
//!
//! cargo example: call_phone --approved (--rdi <12 hex> | --contact <fp or name> [--data-dir <dir>]) [--measure-scan N]
//!                [--video] [--video-backend hardware|libaom] [--seconds 20] [--scan-seconds 30]
use meshlive::{
    bluetooth::{
        self,
        scan::{ScanOptions, Scanner},
    },
    crypto, dial,
    video::DecodeDevice,
};
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};
#[path = "shared/in_call.rs"]
mod in_call;
use in_call::{CallOptions, Res, Stores, arg, e, hex, log, now_ms, run_call};


/// The identity key could not be opened: a setup failure of the link stage.
fn ident_err<E: std::fmt::Debug>(error: E) -> dial::Error {
    dial::Error { stage: dial::Stage::Link, detail: format!("identity {error:?}") }
}

fn main() {
    if let Err(error) = run() {
        log("failed", &error);
        std::process::exit(1);
    }
}

fn run() -> Res<()> {
    if !std::env::args().any(|a| a == "--approved") {
        eprintln!(
            "call_phone uses the real radio. Run only with --approved on test devices you own."
        );
        std::process::exit(2);
    }
    if let Some(seconds) = arg("--measure-scan").and_then(|s| s.parse::<u64>().ok()) {
        // Crowd measurement: the same adverts sampled N s with the default
        // watcher, then N s with SamplingInterval = 0 (every report).
        for every in [false, true] {
            let scanner = Scanner::start_with(ScanOptions { rotating: true, compact: true }, every).map_err(e)?;
            let until = Instant::now() + Duration::from_secs(seconds);
            let never = AtomicBool::new(false);
            while scanner.receive(until, &never).is_ok() {}
            log("stack_fact", &format!("{} every_report={every}", scanner.facts()));
        }
        if arg("--rdi").is_none() && arg("--contact").is_none() {
            log("done", "measure-scan only");
            return Ok(());
        }
    }
    // A stored contact to call by its recognised rotating ID, or a given RDI.
    let contact = match arg("--contact") {
        Some(who) => Some(Contact::find(&who)?),
        None => None,
    };
    let rdi: Option<[u8; 6]> = arg("--rdi")
        .and_then(|h| {
            (0..6)
                .map(|i| u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()
        })
        .and_then(|v| v.try_into().ok());
    let persistent = std::env::args().any(|a| a == "--persistent");
    if rdi.is_none() && contact.is_none() {
        return Err("--rdi needs the 12 hex digits of the advertised rotating ID (or use --contact)".into());
    }
    let video = std::env::args().any(|a| a == "--video");
    let video_backend = arg("--video-backend");
    if let Some(b) = video_backend.as_deref() {
        if b != "hardware" && b != "libaom" {
            return Err("--video-backend is hardware or libaom".into());
        }
    }
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(20);
    let scan_seconds: u64 = arg("--scan-seconds")
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    // CF-HS-50: real keys are compared with the verified contacts.
    meshlive::identity_gate::set_store_dir(
        arg("--data-dir").map(Into::into).or_else(meshlive::sealed::default_dir),
    );
    let caps = bluetooth::probe();
    log(
        "probe",
        &format!("status=\"{}\" central={:?}", caps.status(), caps.central),
    );
    caps.require(false).map_err(e)?;
    log(
        "codecs",
        &format!(
            "decode_mask={:#04x} mf_probe={:?} (bit0 VP9, bit1 AV1; Media Foundation decoders)",
            meshlive::video::decode_mask(),
            meshlive::video::decode_mask_probe().map_err(|e| format!("{e:?}"))
        ),
    );

    // 1. Find the selected rotating call offer.
    log(
        "scan_start",
        &format!(
            "rdi={} contact={} seconds={scan_seconds}",
            rdi.map_or("-".into(), |r| hex(&r)),
            contact.is_some()
        ),
    );
    let scan_started = Instant::now();
    let scanner = Scanner::start(ScanOptions {
        rotating: true,
        compact: true,
    })
    .map_err(e)?;
    let deadline = Instant::now() + Duration::from_secs(scan_seconds);
    let never = AtomicBool::new(false);
    let sighting = loop {
        let s = scanner.receive(deadline, &never).map_err(e)?;
        let a = &s.announcement;
        if !dial::is_call_offer(a) {
            continue;
        }
        match (&contact, rdi) {
            (_, Some(r)) if a.prefix != r => {}
            (Some(c), _) if a.version != 5 || !c.recognises(&a.prefix) => {}
            _ => break s,
        }
    };
    let rdi = sighting.announcement.prefix;
    log("stack_fact", &scanner.facts());
    drop(scanner);
    let (address, address_type) = sighting.route.diagnostic_address();
    log(
        "scan_found",
        &format!(
            "discovery_ms={} rdi={} address={address:012x} type={address_type} rssi={} adv_type={} connectable={} scannable={} flags={:02x} summary={:02x} cost={}",
            scan_started.elapsed().as_millis(),
            hex(&rdi),
            sighting.rssi,
            sighting.kind.0,
            sighting.kind.1,
            sighting.kind.2,
            sighting.announcement.flags,
            sighting.announcement.summary,
            sighting.announcement.cost
        ),
    );

    // 2-4. GATT connection, BMH3 link and endpoint handshakes, reveal gate.
    let cancelled = Arc::new(AtomicBool::new(false));
    let call_id: [u8; 16] = crypto::random_nonce().map_err(e)?[..16].try_into().unwrap();
    // The encoder device exists before the call so Accept is not delayed.
    let owner = if video {
        let (gpu, _, _) = meshlive::video::device(false, false).map_err(e)?;
        Some(DecodeDevice::new(&gpu).map_err(e)?)
    } else {
        None
    };
    let dial_started = Instant::now();
    // CF-CALL-07 / CF-LINK-31: one retry of a setup failure on a fresh
    // sighting of the same offer, inside 15 s; the user-facing reason is
    // logged as failed_text (the app shows the same words).
    let attempt = |sighting: &meshlive::bluetooth::scan::Sighting| -> Result<dial::Dialed, dial::Error> {
        let rdi = sighting.announcement.prefix;
        match &contact {
        Some(c) => {
            let who = c.dial_for(&rdi).map_err(|detail| dial::Error { stage: dial::Stage::Connect, detail })?;
            let (identity, _) = meshlive::identity::Identity::open_or_create().map_err(ident_err)?;
            log("contact_dial", "tag=contact identity=persistent");
            dial::dial_contact(&sighting.route, rdi, &who, &identity, call_id, !video, cancelled.clone(), &mut |p, d| log(p, d))
        }
        None if dial::needs_bmh2(&sighting.announcement) => {
            // A version 2 offer without the BMH3 hint (default iPhone/Mac):
            // BMH2 with the persistent identity (it has no stranger keys).
            log("bmh2_dial", "identity=persistent");
            let (identity, _) = meshlive::identity::Identity::open_or_create().map_err(ident_err)?;
            dial::dial_bmh2(&sighting.route, rdi, &identity, call_id, !video, cancelled.clone(), &mut |p, d| log(p, d))
        }
        None => {
            let real = if persistent {
                log("stranger_dial", "reveal=persistent");
                Some(meshlive::identity::Identity::open_or_create().map_err(ident_err)?.0)
            } else {
                None
            };
            let version5 = sighting.announcement.version == 5;
            dial::dial_selected(&sighting.route, rdi, version5, real, call_id, !video, cancelled.clone(), &mut |p, d| log(p, d))
        }
    }
    };
    let mut result = attempt(&sighting);
    if let Err(error) = &result
        && let Some(wait) = dial::retry_plan(error, dial_started.elapsed(), false)
    {
        log("dial_retry", &format!("after={error} rescan_ms={}", wait.as_millis()));
        let scanner = Scanner::start(ScanOptions { rotating: true, compact: true }).map_err(e)?;
        let until = Instant::now() + wait;
        let fresh = loop {
            match scanner.receive(until, &never) {
                Ok(s)
                    if s.announcement.prefix == sighting.announcement.prefix
                        && s.announcement.version == sighting.announcement.version
                        && dial::is_call_offer(&s.announcement) =>
                {
                    break Some(s);
                }
                Ok(_) => {}
                Err(_) => break None,
            }
        };
        drop(scanner);
        match fresh {
            Some(s) => {
                log("resighted", &format!("ms={}", dial_started.elapsed().as_millis()));
                result = attempt(&s);
            }
            None => log("resight_failed", "the offer was not seen again"),
        }
    }
    let mut dialed = match result {
        Ok(dialed) => dialed,
        Err(error) => {
            log("failed_text", &dial::setup_failure_text(Some(&error)));
            return Err(error.to_string());
        }
    };
    // Phase 2 leases in this call (contact or persistent identity only).
    let stores = if contact.is_some() || persistent { Some(Stores::open()?) } else { None };
    log("connected", &format!("dial_ms={}", dial_started.elapsed().as_millis()));
    log("invite", &format!("call_id={} voice={}", hex(&call_id), !video));
    let result = run_call(&mut dialed, CallOptions { video, owner: owner.as_ref(), video_backend, seconds, started: dial_started, outgoing: true, stores, initial: Vec::new() });
    drop(dialed);
    result
}

/// A stored verified contact and the presence leases that recognise it.
struct Contact {
    node: [u8; 16],
    vault: meshlive::presence::Vault,
    epoch: u32,
}
impl Contact {
    fn find(who: &str) -> Res<Self> {
        let dir = arg("--data-dir").map(Into::into).or_else(meshlive::sealed::default_dir).ok_or("no data folder")?;
        let mut store = meshlive::contacts::ContactStore::open_default(&dir);
        let list = store.list().map_err(e)?;
        let wanted = who.to_ascii_lowercase();
        let matches: Vec<_> = list
            .iter()
            .filter(|c| (wanted.len() >= 8 && hex(&c.fingerprint).starts_with(&wanted)) || c.name == who)
            .collect();
        let [found] = matches.as_slice() else {
            return Err(format!("--contact matches {} stored contacts (need exactly one)", matches.len()));
        };
        let mut vault = meshlive::presence::Vault::open_default(&dir);
        let epoch = dial::wall_epoch().ok_or("clock before 1970")?;
        if !vault.ensure_loaded(epoch) {
            return Err("the presence store cannot be read".into());
        }
        let node = found.node_id();
        let holds = vault.contact(&node).and_then(|c| c.lease.as_ref()).is_some();
        log("contact", &format!("found=1 lease_held={holds}"));
        if !holds {
            return Err("no lease from this contact yet: scan both ways first".into());
        }
        Ok(Self { node, vault, epoch })
    }
    fn recognises(&self, rdi: &[u8; 6]) -> bool {
        self.vault.recognise_call_offer(rdi, self.epoch, now_ms() as i64).contains(&self.node)
    }
    fn dial_for(&self, rdi: &[u8; 6]) -> Res<dial::ContactDial> {
        match self.vault.lease_for(rdi, self.epoch, now_ms() as i64) {
            Some((contact, lease_key)) if contact == self.node => Ok(dial::ContactDial { contact, lease_key }),
            _ => Err("the sighted ID is no longer recognised".into()),
        }
    }
}
