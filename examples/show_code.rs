//! Phone test of the BVMID2 DISPLAYER on this PC (Design QW + QL; run only
//! with `--approved` on test devices you own; it advertises on the
//! real radio). The PC announces its rotating call offer (Design QW) and
//! shows a live code for that same RDI; a phone scans it (Add contact),
//! finds the PC by the announcement, connects and runs both handshakes with
//! the code's tag. The single-use reservation (`qr_display`: replay pins,
//! busy, the attempt cap of 3, the reuse alert, 45 s reservation) decides;
//! the completed end-to-end handshake commits the code and opens the scoped
//! verification session as the displayer: it never offers first and answers
//! the scanner's OFFER with OFFER and LEASE only when this PC's user
//! confirmed this scanner during the current visit of Add contact. Headless,
//! the "visit" is this PC's own scan of that phone's code (scan_code --confirm
//! yes) within `--visit-seconds` (default 600); otherwise the session ends
//! 2 s after the commit and no lease moves. Receive calls is off here: any
//! link that is not a scan of the shown code is closed before any ring.
//!
//! The code changes with the RDI (a new code line is printed) and after
//! 300 s; one completed verification ends the run. Every line has UTC ms:
//! qw_policy, announce_*, gatt_event, code, incoming_subscriber,
//! callee_link_done/callee_endpoint_done, qr_committed, scanned_by, exchange,
//! ended, recognised, done. Stores: the app's own unless `--data-dir`.
//!
//! cargo example: show_code --approved [--seconds 300] [--visit-seconds 600] [--data-dir D]
#[path = "shared/in_call.rs"]
mod in_call;
use in_call::{Res, Stores, arg, e, hex, log, now_ms};
use meshlive::{
    bmh2::SigningIdentity,
    callee::{self, Inbound, Listening, Poll},
    contacts::Method,
    dial,
    qr_display::{Display, Event},
    verify_scan::{Carrier, End, ScanSession, StoreHooks},
};
use std::time::{Duration, Instant};

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(error) => {
            log("failed", &error);
            1
        }
    };
    log("done", "providers stopped; no camera, microphone or audio output was opened");
    std::process::exit(code);
}

fn run() -> Res<()> {
    if !std::env::args().any(|a| a == "--approved") {
        eprintln!("show_code advertises on the real radio. Run only with --approved on test devices you own.");
        std::process::exit(2);
    }
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(300);
    let visit_ms: i64 = arg("--visit-seconds").and_then(|s| s.parse::<i64>().ok()).unwrap_or(600) * 1000;
    let dir = arg("--data-dir").map(Into::into).or_else(meshlive::sealed::default_dir).ok_or("no data folder")?;
    meshlive::identity_gate::set_store_dir(Some(dir));
    let mut trace = |p: &str, d: &str| log(p, d);
    let caps = meshlive::bluetooth::probe();
    log("probe", &format!("status=\"{}\" peripheral={:?} extended={:?}", caps.status(), caps.peripheral, caps.extended_advertising));
    caps.require(true).map_err(e)?;
    callee::policy(&mut trace).map_err(String::from)?;
    let (identity, created) = meshlive::identity::Identity::open_or_create().map_err(e)?;
    let fingerprint = meshlive::crypto::sha256(&SigningIdentity::public_key(&identity).map_err(e)?).map_err(e)?;
    log("identity", &format!("created={created} fingerprint_prefix={}", hex(&fingerprint[..4])));
    let mut stores = Stores::open()?;
    let offer_now = |stores: &mut Stores| -> Option<callee::OwnOffer> {
        let epoch = dial::wall_epoch()?;
        stores.epoch = epoch;
        if !stores.vault.ensure_loaded(epoch) {
            return None;
        }
        callee::own_offer(stores.vault.state()?, epoch)
    };
    // The displayer's reservation state; its clock is this process's monotonic time.
    let clock = Instant::now();
    let mut spent = 0u32;
    let mut display = Display::new(
        Box::new(move || clock.elapsed().as_millis() as u64),
        // The global signing budget for code attempts (bounded per run).
        Box::new(move || {
            spent += 1;
            spent <= 30
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut listen: Option<Listening> = None;
    let mut shown_rdi: Option<[u8; 6]> = None;
    let mut reuse_alert = false;
    let mut guards = Default::default();
    let result = loop {
        if Instant::now() >= deadline {
            break Err(format!("no completed scan within {seconds} s"));
        }
        for event in std::mem::take(&mut display.events) {
            match event {
                Event::ReuseAlert => {
                    reuse_alert = true;
                    log("reuse_alert", "Two phones used your code. The first one may not be the person in front of you. Start again.");
                }
                Event::Ended(why) => log("code_ended", &format!("reason={why}")),
                Event::ScanCompleted(peer) => log("scan_completed", &format!("fingerprint_prefix={}", hex(&peer[..4]))),
            }
        }
        let Some(offer) = offer_now(&mut stores) else {
            break Err("no presence keys for this epoch".into());
        };
        let current = match listen.take() {
            Some(l) => l,
            None => match Listening::open(&offer, &mut trace) {
                Ok(l) => l,
                Err((error, text)) => break Err(format!("{error:?}: {text}")),
            },
        };
        // The code always names the RDI on air (a new one when it changed or ended).
        if shown_rdi != Some(current.rdi) || !display.has_code() {
            display.on_rdi_changed(&current.rdi);
            match display.show(fingerprint, current.rdi, Display::fresh_secret()) {
                Ok(text) => {
                    shown_rdi = Some(current.rdi);
                    log("code", &format!("rdi={} text={text}", hex(&current.rdi)));
                }
                Err(error) => break Err(format!("code {error:?}")),
            }
        }
        let mut current = current;
        match current.poll(&mut || offer_now(&mut stores), &mut trace) {
            Poll::Waiting => {
                listen = Some(current);
                std::thread::sleep(Duration::from_millis(50));
            }
            Poll::Failed(text) => {
                current.stop(&mut trace);
                break Err(text);
            }
            Poll::Client => {
                stores.vault.ensure_loaded(current.epoch);
                let state = stores.vault.state().cloned().ok_or("presence store")?;
                match callee::answer(current, &state, &mut guards, Some(&mut display), false, &mut |_| false, &mut trace) {
                    Err(error) => log("qr_attempt_failed", &error),
                    Ok(Inbound::Call(_)) => log("qr_attempt_failed", "a call while only the code is shown"),
                    Ok(Inbound::Scan { peripheral, reached }) => {
                        break verify(peripheral, reached, &mut stores, &mut display, reuse_alert, visit_ms);
                    }
                }
            }
        }
    };
    if let Some(l) = listen.take() {
        l.stop(&mut trace);
    }
    display.leave();
    result
}

/// The displayer's scoped session after a committed scan.
fn verify(
    mut peripheral: meshlive::bluetooth::peripheral::Peripheral,
    reached: meshlive::verify_scan::Reached,
    stores: &mut Stores,
    display: &mut Display,
    reuse_alert: bool,
    visit_ms: i64,
) -> Res<()> {
    let started = Instant::now();
    let scanner = reached.peer_fingerprint;
    let now = now_ms() as i64;
    let known = stores.contacts.get(&scanner).ok().flatten();
    // "This PC's user confirmed this scanner during the current visit":
    // headless, this PC's own scan of that phone within the visit window.
    let confirmed_in_visit = known
        .as_ref()
        .is_some_and(|c| c.method == Method::QrScan && now.saturating_sub(c.verified_at_ms) <= visit_ms);
    let answering = confirmed_in_visit && !reuse_alert;
    log(
        "scanned_by",
        &format!(
            "fingerprint_prefix={} known_contact={} confirmed_in_visit={confirmed_in_visit} reuse_alert={reuse_alert} answering={answering}",
            hex(&scanner[..4]),
            known.as_ref().map_or("none".into(), |c| format!("\"{}\"", c.name))
        ),
    );
    if confirmed_in_visit {
        // Both K_qr events in one visit: they scanned this PC, this PC scanned them.
        let marked = stores.claims.mark_scanned_both_ways(&scanner, now);
        log("scanned_each_other", &format!("recorded={marked:?}"));
    }
    for event in std::mem::take(&mut display.events) {
        if let Event::ScanCompleted(peer) = event {
            log("scan_completed", &format!("fingerprint_prefix={}", hex(&peer[..4])));
        }
    }
    let clock = Instant::now();
    let now = || clock.elapsed().as_millis() as u64;
    let peer_node = reached.peer_node;
    let (mut session, mut buffer) = ScanSession::displayer(reached, now(), answering);
    let mut logged = 0;
    loop {
        {
            let mut hooks = StoreHooks {
                contacts: &mut stores.contacts,
                vault: &mut stores.vault,
                claims: Some(&mut stores.claims),
                epoch: stores.epoch,
                now_ms: now_ms() as i64,
            };
            let mut input = buffer.as_slice();
            session.on_input(&mut input, now(), &mut hooks);
            let used = buffer.len() - input.len();
            buffer.drain(..used);
        }
        for line in &session.log[logged..] {
            log("exchange", line);
        }
        logged = session.log.len();
        for record in session.take_writes() {
            if let Err(error) = peripheral.send(&record, Instant::now() + Duration::from_secs(8)) {
                log("write_failed", &error);
                session.link_lost();
            }
        }
        for line in peripheral.take_gatt_events() {
            log("gatt_event", &line);
        }
        if let Some(end) = session.poll(now()) {
            for record in session.take_writes() {
                let _ = peripheral.send(&record, Instant::now() + Duration::from_secs(2));
            }
            for line in &session.log[logged..] {
                log("exchange", line);
            }
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
                let recognised = stores.vault.contact(&peer_node).and_then(|c| c.lease.as_ref()).is_some();
                log("recognised", &format!("lease_held={recognised}"));
            }
            break;
        }
        if peripheral.receive(&mut buffer, Duration::from_millis(20)).is_err() {
            session.link_lost();
        }
    }
    peripheral.close();
    Ok(())
}
