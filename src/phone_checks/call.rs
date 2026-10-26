//! Opt-in normal dial/session path with generated silence and discarded PCM.
use super::{Env, Store};
use meshlive::{bluetooth::scan::{Scanner, ScanOptions}, call::Reason, call_session::Notice, media::{Packet, AUDIO, CONFIG}};
use std::{collections::BTreeMap, path::Path, sync::{Arc, atomic::AtomicBool}, time::{Duration, Instant}};

#[derive(Default, Clone, Copy)]
struct PeerPin { route: Option<(u64, i32)>, node: Option<[u8; 16]> }

#[test]
fn opt_in_native_voice_call_phone_check() {
    let Ok(mode) = std::env::var("MESHLIVE_PHONE_CHECK") else { return; };
    if !mode.starts_with("call-") { return; }
    if mode == "call-ui" { return; }
    assert!(matches!(mode.as_str(), "call-voice" | "call-incoming" | "call-busy" | "call-ring" | "call-return-ring"));
    let _mta = meshlive::bluetooth::MtaUsage::for_process().expect("Process MTA");
    let dir = std::env::current_dir().unwrap().join(".artifacts/phone"); std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(dir.join("call-host")).unwrap();
    meshlive::identity_gate::set_store_dir(Some(dir.join("call-host")));
    let _store = Store;
    let admission = if std::env::var("MESHLIVE_PHONE_CALL_MEDIA_GATE").as_deref() == Ok("0") { "0" } else { "1" };
    let _env = Env::set(&[("MESHLIVE_PLAYBACK_GAIN", "0"), ("MESHLIVE_CALL_BUFFERED_INPUT", "1"), ("MESHLIVE_CALL_MEDIA_GATE", admission), ("MESHLIVE_LABELS", "0")]);
    let result = if mode == "call-return-ring" {
        let mut peer = PeerPin::default();
        incoming(&dir, "call-incoming", Some(&mut peer)).and_then(|()| {
            if peer.route.is_none() && peer.node.is_none() { return Err("Controlled incoming peer unavailable".into()); }
            std::thread::sleep(Duration::from_secs(10));
            ringing_only(&dir, Some(peer))
        })
    } else if mode == "call-ring" { ringing_only(&dir, None) }
        else if mode == "call-voice" { outgoing(&dir, None).and_then(|call| media(call, Vec::new(), &dir, &mode, None)) }
        else { incoming(&dir, &mode, None) };
    if let Err(error) = &result {
        std::fs::write(dir.join(format!("{mode}-result.txt")), format!("mode={mode}\nfailure=true\nerror={error}\n")).unwrap();
    }
    assert!(result.is_ok(), "{result:?}");
}

fn outgoing(dir: &Path, bound_peer: Option<PeerPin>) -> Result<meshlive::dial::Dialed, String> {
    let allow_bmh2 = std::env::var("MESHLIVE_PHONE_CALL_BMH2").as_deref() == Ok("1");
    let bound_route = bound_peer.and_then(|p| p.route);
    let legacy_node = (std::env::var("MESHLIVE_PHONE_RETURN_LEGACY_ID").as_deref() == Ok("1"))
        .then(|| bound_peer.and_then(|p| p.node)).flatten();
    let selected = if bound_peer.is_some() { "route".into() } else { std::env::var("MESHLIVE_PHONE_CALL_RDI").map_err(|_| "Select the approved phone's current call RDI")? };
    let parse = |s: &str| -> Result<Vec<u8>, String> {
        if s.len() != 12 || !s.bytes().all(|b| b.is_ascii_hexdigit()) { return Err("Invalid call RDI".into()); }
        Ok((0..12).step_by(2).map(|i| u8::from_str_radix(&s[i..i+2], 16).unwrap()).collect())
    };
    let route = if bound_peer.is_some() { bound_route } else if selected == "route" {
        let value = std::env::var("MESHLIVE_PHONE_CALL_ROUTE").map_err(|_| "Approved public route required")?;
        let bytes = parse(&value)?;
        let address = bytes.into_iter().fold(0u64, |n, b| (n << 8) | u64::from(b));
        if address == 0 || address & 0xffffffffff == 0 { return Err("Unusable public route".into()); }
        Some((address, 0))
    } else { None };
    let legacy_identity = selected == "identity";
    let mut rdi = if legacy_identity {
        let handle = std::env::var("MESHLIVE_PHONE_CALL_IDENTITY").map_err(|_| "Approved phone identity required")?;
        Some(meshlive::names::parse_handle(&handle).ok_or("Invalid identity handle")?.to_vec())
    } else if matches!(selected.as_str(), "select" | "route") { None } else { Some(parse(&selected)?) };
    let cancel = Arc::new(AtomicBool::new(false));
    let scanner = Scanner::start(ScanOptions { rotating: true, compact: true }).map_err(|e| format!("Scan: {e:?}"))?;
    let deadline = Instant::now() + Duration::from_secs(if selected == "select" { 180 } else { 30 });
    let mut seen = BTreeMap::new(); let mut reported = Instant::now(); let selection_started = std::time::SystemTime::now();
    let selected = loop {
        if Instant::now() >= deadline { return Err("Approved call offer was not discovered".into()); }
        match scanner.receive(Instant::now() + Duration::from_millis(100), &cancel) {
            Ok(s) if s.announcement.cost == 0 && meshlive::dial::is_call_offer(&s.announcement)
                && (allow_bmh2 || !meshlive::dial::needs_bmh2(&s.announcement)) => {
                seen.insert(s.announcement.prefix, Instant::now());
                if route.is_some_and(|address| s.route.diagnostic_address() == address) {
                    eprintln!("PHONE_CHECK selected=controlled_route"); break s;
                }
                if s.announcement.version == 2 && legacy_node.is_some_and(|node| s.announcement.prefix == node[..6]) {
                    eprintln!("PHONE_CHECK selected=authenticated_legacy_prefix"); break s;
                }
                if rdi.as_ref().is_some_and(|rdi| s.announcement.prefix == rdi.as_slice())
                    && (!legacy_identity || s.announcement.version == 2) { break s; }
            }
            Ok(_) | Err(meshlive::bluetooth::Error::Timeout) => {},
            Err(e) => return Err(format!("Scan: {e:?}")),
        }
        if reported.elapsed() >= Duration::from_secs(1) {
            seen.retain(|_, at| at.elapsed() < Duration::from_secs(8));
            let offers = seen.keys().map(|id: &[u8; 6]| id.iter().map(|b| format!("{b:02x}")).collect::<String>()).collect::<Vec<_>>().join("\n");
            std::fs::write(dir.join("call-voice-offers.txt"), offers).map_err(|_| "Offer write")?;
            let selection = dir.join("call-voice-selection.txt");
            let fresh = std::fs::metadata(&selection).and_then(|m| m.modified()).is_ok_and(|at| at >= selection_started);
            if selected == "select" && fresh && let Ok(choice) = std::fs::read_to_string(selection) { rdi = Some(parse(choice.trim())?); }
            reported = Instant::now();
        }
    };
    drop(scanner);
    let call_id = meshlive::crypto::random_nonce().map_err(|e| format!("Random: {e:?}"))?[..16].try_into().unwrap();
    let dial_started = Instant::now();
    let mut trace = |phase: &str, _: &str| eprintln!("PHONE_CHECK call phase={phase} elapsed_ms={}", dial_started.elapsed().as_millis());
    if meshlive::dial::needs_bmh2(&selected.announcement) {
        let identity = meshlive::identity::Identity::ephemeral().map_err(|_| "Test identity")?;
        meshlive::dial::dial_bmh2(&selected.route, selected.announcement.prefix, &identity, call_id, true, cancel, &mut trace)
    } else {
        meshlive::dial::dial_selected(&selected.route, selected.announcement.prefix, selected.announcement.version == 5, None, call_id, true, cancel, &mut trace)
    }.map_err(|e| format!("Dial: {e}"))
}

/// A bounded central/setup check. It never answers, starts media or opens
/// capture/output, and uses ordinary authenticated Ringing then Cancel.
fn ringing_only(dir: &Path, peer: Option<PeerPin>) -> Result<(), String> {
    let repeat = std::env::var("MESHLIVE_PHONE_CALL_REPEAT").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(1).clamp(1, 3);
    for attempt in 1..=repeat {
        eprintln!("PHONE_CHECK ring attempt={attempt} started=true");
        let started = Instant::now();
        let result = (|| -> Result<(), String> {
            let mut call = outgoing(dir, peer)?;
            if let Some(expected) = peer.and_then(|p| p.node) {
                if call.peer_real.is_some_and(|(node, _)| node != expected) { return Err("Controlled peer identity changed".into()); }
            }
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                call.flush().map_err(|e| format!("Write: {e}"))?;
                let notices = call.pump_input(Duration::from_millis(10)).map_err(|e| format!("Read: {e}"))?;
                if notices.iter().any(|n| matches!(n, Notice::StartMedia { .. })) {
                    let _ = call.session.hang_up(Reason::Normal); let _ = call.flush();
                    return Err("Unexpected answer during ringing-only check".into());
                }
                let rang = call.session.take_remote().iter().any(|m| m.signal == meshlive::call::Signal::Ringing);
                if rang {
                    call.session.cancel().map_err(|e| format!("Cancel: {e:?}"))?;
                    call.flush().map_err(|e| format!("Cancel write: {e}"))?;
                    std::thread::sleep(Duration::from_millis(300));
                    return Ok(());
                }
                if notices.iter().any(|n| matches!(n, Notice::Ended(_))) { return Err("Peer ended before Ringing".into()); }
                if Instant::now() >= deadline { return Err("Ringing deadline".into()); }
            }
        })();
        let summary = format!("mode=call-ring\nattempt={attempt}\nringing={}\nsetup_and_cancel_ms={}\nmedia_started=false\ncancel_flushed={}\n", result.is_ok(), started.elapsed().as_millis(), result.is_ok());
        std::fs::write(dir.join(format!("call-ring-attempt-{attempt}.txt")), summary).map_err(|_| "Result write")?;
        result?;
        if attempt < repeat { std::thread::sleep(Duration::from_secs(10)); }
    }
    std::fs::write(dir.join("call-ring-result.txt"), format!("mode=call-ring\nattempts={repeat}\nringing=true\nmedia_started=false\ncancel_flushed=true\n")).map_err(|_| "Result write".into())
}

fn incoming(dir: &Path, mode: &str, mut return_peer: Option<&mut PeerPin>) -> Result<(), String> {
    use meshlive::{bmh2::SigningIdentity, callee::{self, Listening, Poll}, identity::Identity, presence, rotating};
    let mut trace = |phase: &str, _: &str| eprintln!("PHONE_CHECK incoming phase={phase}");
    callee::policy(&mut trace).map_err(str::to_owned)?;
    let identity = Identity::ephemeral().map_err(|e| format!("Identity: {e:?}"))?;
    let node = meshlive::crypto::sha256(&identity.public_key().map_err(|e| format!("Public key: {e:?}"))?).map_err(|e| format!("Node: {e:?}"))?[..16].try_into().unwrap();
    let offer_now = |state: &mut presence::State| -> Option<callee::OwnOffer> {
        let epoch = meshlive::dial::wall_epoch()?;
        let rdi = presence::own_rdi(&state.keys, &node, epoch, rotating::ROLE_CALL_OFFER)?;
        let payload = meshlive::pc_announcement::CallOffer { rdi, relay: false, consent: false, codecs: 3, cost: 0 }.encode().ok()?;
        Some(callee::OwnOffer { epoch, rdi, payload, extra: None })
    };
    let mut state = presence::State::fresh(meshlive::dial::wall_epoch().ok_or("Clock")?);
    let offer = offer_now(&mut state).ok_or("Call offer")?;
    let write_label = |rdi: &[u8; 6]| -> Result<(), String> {
        let label = meshlive::names::stranger_name(meshlive::names::NameInput::Rdi(rdi)).ok_or("Call label")?.name;
        std::fs::write(dir.join(format!("{mode}-ready.txt")), format!("ready=true\nlabel={label}\n")).map_err(|_| "Ready write".into())
    };
    let mut listen = Listening::open(&offer, &mut trace).map_err(|(_, text)| text)?;
    write_label(&listen.rdi)?;
    let mut reported_rdi = listen.rdi;
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        match listen.poll(&mut || offer_now(&mut state), &mut trace) {
            Poll::Client => break,
            Poll::Failed(text) => return Err(text),
            Poll::Waiting => {},
        }
        if listen.rdi != reported_rdi { write_label(&listen.rdi)?; reported_rdi = listen.rdi; }
        if Instant::now() >= deadline { listen.stop(&mut trace); return Err("No phone caller".into()); }
        std::thread::sleep(Duration::from_millis(20));
    }
    let busy = mode == "call-busy";
    let search = busy.then_some(callee::Search { real_prefix: None });
    let callee::Inbound::Call(mut answer) = callee::answer_with_identity(listen, &state, &mut Default::default(), None, true, search, &mut |_| false, identity, &mut trace)? else { return Err("Unexpected code scan".into()) };
    if answer.busy != busy { return Err("Unexpected Busy admission".into()); }
    if let Some(peer) = return_peer.as_deref_mut() {
        peer.route = match &mut answer.dialed.link { meshlive::dial::Link::Peripheral(p) => p.diagnostic_peer_route(), _ => None };
        peer.node = answer.dialed.peer_real.map(|(node, _)| node);
        eprintln!("PHONE_CHECK incoming return_peer_available={}", peer.route.is_some());
    }
    answer.dialed.flush().map_err(|e| format!("Ringing/Busy: {e}"))?;
    if busy {
        if answer.dialed.session.answered() || answer.dialed.session.phase() != meshlive::call::Phase::Ended { return Err("Busy started media".into()); }
        std::fs::write(dir.join("call-busy-result.txt"), "mode=call-busy\nbusy=true\nringing=false\nmedia_started=false\ndecline_flushed=true\n").map_err(|_| "Result write")?;
        // Let the already-submitted ordered write reach the caller before release.
        std::thread::sleep(Duration::from_millis(300));
        return Ok(());
    }
    answer.dialed.pump_input(Duration::from_millis(300)).map_err(|e| format!("Voice offer: {e}"))?;
    if !answer.dialed.session.voice_offer() { return Err("Use the phone's voice-call action".into()); }
    let notices = answer.dialed.session.accept(true).map_err(|e| format!("Accept: {e:?}"))?;
    media(answer.dialed, notices, dir, mode, return_peer)
}

fn media(mut call: meshlive::dial::Dialed, mut pending: Vec<Notice>, dir: &Path, mode: &str, mut return_peer: Option<&mut PeerPin>) -> Result<(), String> {
    let seconds = std::env::var("MESHLIVE_PHONE_SECONDS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(30).clamp(20, 90);
    let mut encoder = meshlive::opus::Encoder::new().map_err(|e| format!("Opus: {e:?}"))?;
    let mut video = meshlive::viewer::DiscardFrames::default();
    let mut pcm = [0; meshlive::opus::Decoder::MAX_SAMPLES];
    let (mut active, mut rang, mut peer_header, mut revealed) = (None::<Instant>, mode == "call-incoming", false, call.peer_real.is_some());
    let (mut sent, mut heard) = (0u64, 0usize);
    let started = Instant::now(); let mut report = started;
    loop {
        call.flush().map_err(|e| format!("Write: {e}"))?;
        let notices = call.pump_input(Duration::from_millis(5)).map_err(|e| format!("Read: {e}"))?;
        for signal in call.session.take_remote() { rang |= signal.signal == meshlive::call::Signal::Ringing; }
        pending.extend(notices);
        for notice in pending.drain(..) {
            match notice {
                Notice::PeerRevealed { node_id, .. } => {
                    revealed = true;
                    if let Some(peer) = return_peer.as_deref_mut() { peer.node = Some(node_id); }
                }
                Notice::PeerHeader(_) => peer_header = true,
                Notice::StartMedia { camera } => {
                    if camera { return Err("Voice call requested local camera".into()); }
                    if active.is_some() { return Err("Repeated media start".into()); }
                    active = Some(Instant::now()); call.session.media_ready().map_err(|e| format!("Media: {e:?}"))?;
                    call.session.send(Packet { flags: AUDIO | CONFIG, pts_us: 0, payload: &encoder.config() }.encode().unwrap()).map_err(|e| format!("Config: {e:?}"))?;
                    eprintln!("PHONE_CHECK call active=true");
                }
                Notice::Ended(reason) => return Err(format!("Peer ended before local Hang-up: {reason:?}")),
                _ => {},
            }
        }
        if let Some(at) = active {
            while u128::from(sent) * 20 < at.elapsed().as_millis() {
                let unit = encoder.encode(&[0; 960]).map_err(|e| format!("Encode: {e:?}"))?;
                if !call.session.send(Packet { flags: AUDIO, pts_us: sent as i64 * 20_000, payload: &unit }.encode().unwrap()).map_err(|e| format!("Audio: {e:?}"))? { return Err("Voice input refused".into()); }
                sent += 1;
            }
        }
        let now = call.now_ns() as i64;
        call.session.pump_video(&mut video, now, 20_000_000).map_err(|e| format!("Video: {e:?}"))?;
        call.session.audio().pump(now, 0, &mut pcm, |_, samples| heard += samples.len()).map_err(|e| format!("Decode: {e:?}"))?;
        if report.elapsed() >= Duration::from_secs(2) {
            let progress = format!("seconds={}\nringing={rang}\nactive={}\npeer_header={peer_header}\nrevealed={revealed}\naudio_sent={sent}\npcm_discarded={heard}\n", started.elapsed().as_secs(), active.is_some());
            std::fs::write(dir.join(format!("{mode}-progress.txt")), &progress).map_err(|_| "Progress write")?;
            eprintln!("PHONE_CHECK call {progress}"); report = Instant::now();
        }
        if active.is_some_and(|at| at.elapsed() >= Duration::from_secs(seconds)) {
            call.session.hang_up(Reason::Normal).map_err(|e| format!("Hang-up: {e:?}"))?;
            call.flush().map_err(|e| format!("Hang-up write: {e}"))?;
            let result = format!("mode={mode}\nmedia_gate={}\nseconds={seconds}\nringing={rang}\nrevealed={revealed}\npeer_header={peer_header}\naudio_sent={sent}\npcm_discarded={heard}\nhangup_flushed=true\n", meshlive::call::media_gate_enabled());
            std::fs::write(dir.join(format!("{mode}-result.txt")), &result).map_err(|_| "Result write")?;
            eprintln!("PHONE_CHECK call result\n{result}");
            if !rang || !revealed || !peer_header || sent < seconds * 40 || heard < 48_000 { return Err("Incomplete voice media flow".into()); }
            return Ok(());
        }
        if started.elapsed() >= Duration::from_secs(100) { return Err("Call did not become active in time".into()); }
    }
}
