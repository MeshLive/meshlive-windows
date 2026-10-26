//! Phone test of the BVMID2 scanner with Design QL (run only with
//! `--approved` on test devices you own; it scans and connects on the
//! real radio). The phone shows its code on the Add contact screen; this tool
//! takes the code text, finds the phone's rotating call offer (10 s, every
//! address), runs both K_qr-tagged handshakes with this PC's persistent
//! identity, and then:
//! - with `--confirm yes`: stores the shield (verified, scanned) under
//!   `--name` and starts the lease exchange (scanner first);
//! - with `--confirm no`, or without `--confirm`: answers No and stores nothing.
//!
//! Stores: the app's own (`%LOCALAPPDATA%\MeshLive`) unless `--data-dir`.
//! Every phase is logged with UTC milliseconds; no camera, microphone or
//! audio output is opened.
//!
//! cargo example: scan_code --approved --code <BVMID2 text> [--confirm yes|no]
//!                [--name <contact name>] [--data-dir <dir>]
use meshlive::{
    bluetooth,
    bvmid2::{self, Code},
    contacts::ContactStore,
    dial,
    identity::Identity,
    presence::Vault,
    verify_scan::{self, Carrier, End, ScanSession, StoreHooks},
};
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}
fn log(phase: &str, detail: &str) {
    println!("t={} phase={phase} {detail}", now_ms());
}
fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}
fn e<E: std::fmt::Debug>(error: E) -> String {
    format!("{error:?}")
}

fn main() {
    if let Err(error) = run() {
        log("failed", &error);
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    if !std::env::args().any(|a| a == "--approved") {
        eprintln!("scan_code uses the real radio. Run only with --approved on test devices you own.");
        std::process::exit(2);
    }
    let text = arg("--code").ok_or("--code needs the BVMID2 text the phone shows")?;
    let code = Code::parse(&text).map_err(|r| format!("not a BVMID2 code: {}", r.reason()))?;
    let yes = match arg("--confirm").as_deref() {
        Some("yes") => true,
        Some("no") | None => false,
        Some(_) => return Err("--confirm is yes or no".into()),
    };
    let name = arg("--name").unwrap_or_default();
    if yes && name.trim().is_empty() {
        return Err("--confirm yes needs --name (the contact's name on this PC)".into());
    }
    let dir = arg("--data-dir").map(Into::into).or_else(meshlive::sealed::default_dir).ok_or("no data folder")?;
    meshlive::identity_gate::set_store_dir(Some(dir.clone()));
    let caps = bluetooth::probe();
    log("probe", &format!("status=\"{}\" central={:?}", caps.status(), caps.central));
    caps.require(false).map_err(e)?;
    let (identity, created) = Identity::open_or_create().map_err(e)?;
    log("identity", &format!("created={created}"));
    let mut contacts = ContactStore::open_default(&dir);
    let mut vault = Vault::open_default(&dir);
    let mut claims = meshlive::claims::ClaimsStore::open_default(&dir);
    let epoch = dial::wall_epoch().ok_or("clock before 1970")?;
    log("vault", &format!("persistent={}", vault.ensure_loaded(epoch)));

    let started = Instant::now();
    log("search_start", &format!("seconds={}", bvmid2::SEARCH_MS / 1000));
    let cancelled = Arc::new(AtomicBool::new(false));
    let (mut central, reached) =
        verify_scan::find_and_reach(&code, &identity, epoch, cancelled, &mut |p, d| log(p, d))
            .map_err(|error| error.to_string())?;
    log(
        "reached",
        &format!("ms={} negotiated={} fingerprint_matches=true", started.elapsed().as_millis(), reached.negotiated),
    );
    let clock = Instant::now();
    let now = || clock.elapsed().as_millis() as u64;
    let (mut session, mut buffer) = ScanSession::new(reached, now());
    let mut answered = false;
    let mut logged = 0;
    loop {
        {
            let mut hooks = StoreHooks {
                contacts: &mut contacts,
                vault: &mut vault,
                claims: Some(&mut claims),
                epoch,
                now_ms: now_ms() as i64,
            };
            let mut input = buffer.as_slice();
            session.on_input(&mut input, now(), &mut hooks);
            let used = buffer.len() - input.len();
            buffer.drain(..used);
            if !answered && session.ended().is_none() {
                answered = true;
                log("question", &bvmid2::confirmation_text(if name.is_empty() { "this phone" } else { &name }));
                let stored = yes && verify_scan::store_shield(hooks.contacts, &session, &code, &name, now_ms() as i64);
                log("answer", &format!("yes={yes} shield_stored={stored}"));
                session.confirm(yes, stored, now(), &mut hooks);
            }
        }
        for line in &session.log[logged..] {
            log("exchange", line);
        }
        logged = session.log.len();
        for record in session.take_writes() {
            if let Err(error) = central.send(&record, Instant::now() + Duration::from_secs(8)) {
                log("write_failed", &error);
                session.link_lost();
            }
        }
        if let Some(end) = session.poll(now()) {
            for record in session.take_writes() {
                let _ = central.send(&record, Instant::now() + Duration::from_secs(2));
            }
            for fact in central.stack_facts() {
                log("stack_fact", &fact);
            }
            central.drain_events(&mut |p, d| log(p, d));
            log(
                "ended",
                &format!(
                    "reason={end:?} ms={} lease_stored={} violations={}",
                    started.elapsed().as_millis(),
                    session.lease_stored(),
                    session.violations()
                ),
            );
            if end == End::Finished && session.lease_stored() {
                let recognised = vault.contact(&session.peer_node).and_then(|c| c.lease.as_ref()).is_some();
                log("recognised", &format!("lease_held={recognised}"));
            }
            break;
        }
        if central.receive(&mut buffer, Duration::from_millis(20)).is_err() {
            session.link_lost();
        }
    }
    log("done", "no camera, microphone or audio output was opened");
    Ok(())
}
