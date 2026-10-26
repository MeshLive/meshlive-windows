//! Headless callee for phone tests of Design QW (run only with `--approved`
//! on test devices you own; it advertises on the real radio). It
//! runs the app's callee path (`meshlive::callee`): the platform policy, the
//! announcement (connectable extended advert, 9-byte call offer under
//! c8d5a10a, restarted at every rotating-ID change after the boundary
//! silence), the responder handshakes for the first phone that subscribes,
//! its Invite, then `--accept` (default) or `--decline`. An accepted call
//! runs `--seconds` of media with the same loop and `stats` lines as
//! call_phone (a quiet generated tone; a generated test pattern if the phone
//! asks for video; `--mic` / `--camera` as call_phone), then hangs up.
//!
//! Every line carries UTC ms: qw_policy, announce_start/status/boundary/
//! restart/stop, gatt_event (each subscription and the first write per
//! characteristic on c8d5a105 and c8d5a10a: WinRT has no service-discovery
//! event, so these show which service a phone found), incoming_subscriber,
//! incoming_handshake, incoming_invite, incoming_ring, accepted/declined,
//! active, stats, hang_up/ended_by_peer, done.
//!
//! `--wait N` (default 600) bounds the wait for a caller; `--data-dir D`
//! uses another app folder (presence keys, contacts). The providers are
//! stopped on every exit path.
//!
//! cargo example: answer_phone --approved [--seconds 20] [--accept|--decline] [--wait 600] [--data-dir D] [--mic] [--camera]
#[path = "shared/in_call.rs"]
mod in_call;
use in_call::{CallOptions, Res, Stores, arg, e, hex, log, run_call};
use meshlive::{
    call::Reason,
    callee::{self, Listening, Poll},
    dial,
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
    log("done", "providers stopped; nothing was recorded");
    std::process::exit(code);
}

fn run() -> Res<()> {
    if !std::env::args().any(|a| a == "--approved") {
        eprintln!("answer_phone advertises on the real radio. Run only with --approved on test devices you own.");
        std::process::exit(2);
    }
    let decline = std::env::args().any(|a| a == "--decline");
    let seconds: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(20);
    let wait: u64 = arg("--wait").and_then(|s| s.parse().ok()).unwrap_or(600);
    let dir = arg("--data-dir").map(Into::into).or_else(meshlive::sealed::default_dir).ok_or("no data folder")?;
    // CF-HS-50: real keys are compared with the verified contacts.
    meshlive::identity_gate::set_store_dir(Some(dir));
    let mut trace = |p: &str, d: &str| log(p, d);
    let caps = meshlive::bluetooth::probe();
    log(
        "probe",
        &format!("status=\"{}\" peripheral={:?} extended={:?} max_adv={:?}", caps.status(), caps.peripheral, caps.extended_advertising, caps.max_advertisement_bytes),
    );
    caps.require(true).map_err(e)?;
    callee::policy(&mut trace).map_err(String::from)?;

    // The presence keys give this epoch's call offer (the app's own store).
    let mut stores = Stores::open()?;
    let offer_now = |stores: &mut Stores| -> Option<callee::OwnOffer> {
        let epoch = dial::wall_epoch()?;
        stores.epoch = epoch;
        if !stores.vault.ensure_loaded(epoch) {
            return None;
        }
        callee::own_offer(stores.vault.state()?, epoch)
    };
    let offer = offer_now(&mut stores).ok_or("no presence keys for this epoch")?;
    let mut listen = Listening::open(&offer, &mut trace).map_err(|(error, text)| format!("{error:?}: {text}"))?;
    log("listening", &format!("wait_s={wait} accept={}", !decline));

    let deadline = Instant::now() + Duration::from_secs(wait);
    loop {
        if Instant::now() >= deadline {
            listen.stop(&mut trace);
            return Err(format!("no caller within {wait} s"));
        }
        match listen.poll(&mut || offer_now(&mut stores), &mut trace) {
            Poll::Waiting => std::thread::sleep(Duration::from_millis(50)),
            Poll::Failed(text) => {
                listen.stop(&mut trace);
                return Err(text);
            }
            Poll::Client => break,
        }
    }
    let subscribed = Instant::now();
    let epoch = listen.epoch;
    stores.vault.ensure_loaded(epoch);
    let state = stores.vault.state().cloned().ok_or("presence store")?;
    let mut guards = Default::default();
    // The answer owns the providers from here; dropping it stops them.
    let callee::Inbound::Call(mut answer) = callee::answer(listen, &state, &mut guards, None, true, &mut |_| false, &mut trace)? else {
        return Err("a code scan without a shown code".into());
    };
    let voice = answer.dialed.session.voice_offer();
    log(
        "incoming_ring",
        &format!(
            "call_id={} verified={} voice={voice} subscriber_to_ring_ms={}",
            hex(&answer.invite.call_id),
            answer.verified,
            subscribed.elapsed().as_millis()
        ),
    );
    // The session queued Ringing; let it out before answering.
    answer.dialed.flush().map_err(|error| error.to_string())?;
    let _ = answer.dialed.pump_input(Duration::from_millis(300));
    let voice = answer.dialed.session.voice_offer();
    if decline {
        answer.dialed.session.decline(Reason::Rejected).map_err(e)?;
        answer.dialed.flush().map_err(|error| error.to_string())?;
        log("declined", "reason=Rejected");
        std::thread::sleep(Duration::from_millis(300));
        return Ok(());
    }
    let notices = answer.dialed.session.accept(voice).map_err(e)?;
    log("accepted", &format!("voice={voice} ring_ms={}", subscribed.elapsed().as_millis()));
    let video = !voice;
    let owner = if video {
        let (gpu, _, _) = meshlive::video::device(false, false).map_err(e)?;
        Some(meshlive::video::DecodeDevice::new(&gpu).map_err(e)?)
    } else {
        None
    };
    // The accept's own notices (StartMedia) go through the same loop.
    let result = run_call(
        &mut answer.dialed,
        CallOptions { video, owner: owner.as_ref(), video_backend: arg("--video-backend"), seconds, started: subscribed, outgoing: false, stores: Some(stores), initial: notices },
    );
    drop(answer);
    result
}

