//! The real winit app and real backend, opt-in and unit-test executable only.
use super::Env;
use crate::{App, Event, Options};
use std::time::{Duration, Instant};
use winit::{event_loop::EventLoop, platform::windows::EventLoopBuilderExtWindows};

#[test]
fn opt_in_native_window_voice_call_check() {
    if std::env::var("MESHLIVE_PHONE_CHECK").as_deref() != Ok("call-ui") { return; }
    assert!(meshlive::app_mode::current().is_review(), "Native window checks require the isolated review profile");
    let _mta = meshlive::bluetooth::MtaUsage::for_process().expect("Process MTA");
    let repeat = std::env::var("MESHLIVE_PHONE_UI_REPEAT").as_deref() == Ok("2");
    assert!(!repeat || ["MESHLIVE_PHONE_UI_RING_TIMEOUT", "MESHLIVE_PHONE_UI_TALK"].iter()
        .all(|key| std::env::var(key).as_deref() != Ok("1")), "Repeat check uses accepted voice calls only");
    let _env = Env::set(&[("MESHLIVE_NATIVE_UI_CHECK", "1"), ("MESHLIVE_PLAYBACK_GAIN", "0"), ("MESHLIVE_PRIVATE_DIAGNOSTICS", "1"),
        ("MESHLIVE_CALL_MEDIA_FACTS", "1"), ("MESHLIVE_CALL_MEDIA_GATE", "1"),
        ("MESHLIVE_CALL_CAPTURE_FENCE", "1"), ("MESHLIVE_CALL_BUFFERED_INPUT", "1"),
        ("MESHLIVE_CALL_FRAGMENT_ORDER", "1"), ("MESHLIVE_GATT_CANCEL_OWNERSHIP", "1")]);
    let _talk = (std::env::var("MESHLIVE_PHONE_UI_TALK").as_deref() == Ok("1"))
        .then(|| Env::set(&[("MESHLIVE_CALL_TALK_HOLD", "1")]));
    let _form = (std::env::var("MESHLIVE_PHONE_UI_PC_FORM").as_deref() == Ok("1"))
        .then(|| Env::set(&[("MESHLIVE_COMPACT_OFFER", "0"), ("MESHLIVE_QW_FORM", "1")]));
    let _store = super::Store;
    let mut builder = EventLoop::<Event>::with_user_event();
    builder.with_any_thread(true);
    let event_loop = builder.build().unwrap();
    let mut app = App {
        options: Options { bluetooth: true, exit: Some(Duration::from_secs(if repeat { 150 } else { 90 })), ..Options::default() },
        proxy: event_loop.create_proxy(), view: None, repaint: None, started: Instant::now(), frames: Vec::new(),
        error: None, last_draw: None, occluded: None, focused: true, last_background: Instant::now(),
        background_ticks: 0, tray: None, start_hidden: false,
    };
    crate::display_state::start();
    event_loop.run_app(&mut app).unwrap();
    assert!(app.error.is_none(), "Native window failed");
    let result = app.view.as_ref().ok_or("Native window did not start").and_then(|view| view.ui.native_check_result().map_err(|_| "Native call flow failed"));
    assert!(result.is_ok(), "{result:?}");
}
