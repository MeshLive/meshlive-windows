//! Opt-in native-window driver, compiled in the unit-test executable only.
use super::{Phase, Ui};
use std::{path::PathBuf, time::{Duration, Instant}};

pub(super) struct State {
    dir: PathBuf,
    armed: bool,
    label: Option<String>,
    ringing: Option<Instant>,
    active: Option<Instant>,
    ended: Option<Instant>,
    voice_only: bool,
    details_hidden: bool,
    muted: bool,
    unmuted: bool,
    remuted: bool,
    end_requested: bool,
    mic_on: bool,
    mic_unavailable: bool,
    talk_check: bool,
    pressed: bool,
    released: bool,
    saw_reopening: bool,
    saw_talking: bool,
    talk_started: Option<Instant>,
    talk_ready_ms: Option<u128>,
    talk_lost_while_held: bool,
    input_interrupted: bool,
    talk_id: Option<egui::Id>,
    talk_id_changed: bool,
    talk_position: Option<egui::Pos2>,
    pointer_down: bool,
    focus_requested: bool,
    ring_timeout: bool,
    missed: bool,
    ring_ms: Option<u128>,
    round: u8,
    rounds: u8,
}
impl State {
    pub(super) fn new() -> Self {
        Self { dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".artifacts/phone"), armed: false,
            label: None, ringing: None, active: None, ended: None, voice_only: true, details_hidden: true,
            muted: false, unmuted: false, remuted: false, end_requested: false, mic_on: false, mic_unavailable: false,
            talk_check: std::env::var("MESHLIVE_PHONE_UI_TALK").as_deref() == Ok("1"),
            pressed: false, released: false, saw_reopening: false, saw_talking: false,
            talk_started: None, talk_ready_ms: None, talk_lost_while_held: false,
            input_interrupted: false, talk_id: None, talk_id_changed: false,
            talk_position: None, pointer_down: false, focus_requested: false,
            ring_timeout: std::env::var("MESHLIVE_PHONE_UI_RING_TIMEOUT").as_deref() == Ok("1"),
            missed: false, ring_ms: None, round: 1,
            rounds: if std::env::var("MESHLIVE_PHONE_UI_REPEAT").as_deref() == Ok("2") { 2 } else { 1 } }
    }
    pub(super) fn take_focus_request(&mut self) -> bool {
        if !self.talk_check || self.active.is_none() || self.focus_requested { return false; }
        self.focus_requested = true;
        true
    }
    pub(super) fn talk_position(&mut self, id: egui::Id, position: egui::Pos2) {
        self.talk_id_changed |= self.pointer_down && self.talk_id.is_some_and(|old| old != id);
        self.talk_id = Some(id);
        self.talk_position = Some(position);
    }
    pub(super) fn input(&mut self, input: &egui::RawInput) -> Vec<egui::Event> {
        if self.pointer_down {
            self.input_interrupted |= !input.focused || input.events.iter().any(|event| matches!(event,
                egui::Event::PointerGone | egui::Event::WindowFocused(false) | egui::Event::PointerButton { pressed: false, .. }));
        }
        let held = self.talk_check && self.active.is_some_and(|at| (6..9).contains(&at.elapsed().as_secs()))
            && self.ended.is_none() && !self.end_requested;
        if held == self.pointer_down { return Vec::new(); }
        let Some(pos) = self.talk_position else { return Vec::new() };
        self.pointer_down = held;
        vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton {
            pos, button: egui::PointerButton::Primary, pressed: held, modifiers: egui::Modifiers::NONE,
        }]
    }
    pub(super) fn tick(&mut self, ui: &mut Ui) {
        if !self.armed {
            ui.switches.onboarded = true;
            ui.switches.receive_calls = true;
            ui.switches.voice_calls = true;
            ui.switches.block_unknown = false;
            ui.switches_changed();
            self.armed = true;
        }
        if ui.pc_visible && ui.own_generated != self.label {
            if let Some(label) = &ui.own_generated {
                let _ = std::fs::create_dir_all(&self.dir);
                let round = if self.rounds == 2 { format!("round={}\n", self.round) } else { String::new() };
                let _ = std::fs::write(self.dir.join("call-ui-ready.txt"), format!("ready=true\nlabel={label}\n{round}"));
                self.label = Some(label.clone());
            }
        }
        let mut snapshot = None;
        ui.with_call(|c| snapshot = Some((c.phase(), c.voice, c.camera, c.peer_camera)));
        let Some((phase, voice, camera, peer_camera)) = snapshot else { return };
        match phase {
            Phase::IncomingRinging => {
                let since = self.ringing.get_or_insert_with(Instant::now);
                if !self.ring_timeout && since.elapsed() >= Duration::from_secs(2) {
                    ui.with_call(|c| { let _ = if c.voice { c.accept(true) } else { c.decline() }; });
                }
            }
            Phase::Active => {
                let age = self.active.get_or_insert_with(Instant::now).elapsed();
                self.mic_on |= matches!(ui.call_mic, Some(None));
                self.mic_unavailable |= matches!(ui.call_mic, Some(Some(_)));
                if age >= Duration::from_secs(1) {
                    self.voice_only &= voice && !camera && !peer_camera;
                    self.details_hidden &= ui.call_media_lines().is_empty();
                }
                if age >= Duration::from_secs(2) && !self.muted {
                    ui.with_call(|c| { if c.toggle_mute().is_ok() { self.muted = c.muted; } });
                }
                if age >= Duration::from_secs(4) && !self.unmuted {
                    ui.with_call(|c| { if c.toggle_mute().is_ok() { self.unmuted = !c.muted; } });
                }
                if age >= Duration::from_secs(5) && !self.remuted {
                    ui.with_call(|c| { if c.toggle_mute().is_ok() { self.remuted = c.muted; } });
                }
                if self.talk_check {
                    if age >= Duration::from_secs(6) && !self.pressed {
                        self.pressed = ui.call.as_ref().is_some_and(|c| c.talking && c.sending_audio());
                        if self.pressed { self.talk_started = Some(Instant::now()); }
                    }
                    if self.pressed && !self.released {
                        self.saw_reopening |= ui.call_mic_restarting;
                        if age < Duration::from_secs(9) {
                            self.talk_lost_while_held |= ui.call.as_ref().is_some_and(|c| !c.talking || !c.sending_audio());
                        }
                        if self.saw_reopening && !ui.call_mic_restarting && matches!(ui.call_mic, Some(None)) && self.talk_ready_ms.is_none() {
                            self.talk_ready_ms = self.talk_started.map(|at| at.elapsed().as_millis());
                        }
                        self.saw_talking |= self.saw_reopening && !ui.call_mic_restarting && matches!(ui.call_mic, Some(None))
                            && ui.call.as_ref().is_some_and(|c| c.talking && c.sending_audio());
                    }
                    if age >= Duration::from_secs(9) && !self.released {
                        self.released = self.pressed && ui.call.as_ref().is_some_and(|c| !c.talking && !c.sending_audio());
                    }
                }
                if age >= Duration::from_secs(if self.talk_check { 10 } else { 8 }) && !self.end_requested {
                    ui.with_call(|c| { self.end_requested = c.end().is_ok(); });
                }
            }
            Phase::Ended => {
                if self.ended.is_none() {
                    self.ring_ms = self.ringing.map(|at| at.elapsed().as_millis());
                    self.missed = ui.call_end == Some(super::backend::CallEndReason::Missed)
                        && ui.call_status().as_deref() == Some("Missed call");
                }
                self.ended.get_or_insert_with(Instant::now);
            }
            _ => {},
        }
        if self.round < self.rounds && self.ended.is_some_and(|at| at.elapsed() >= Duration::from_secs(3)) && self.passed() {
            if std::fs::write(self.dir.join(format!("call-ui-round-{}.txt", self.round)), self.report(true)).is_err() { return; }
            ui.close_call();
            let round = self.round + 1;
            let mut next = Self::new();
            next.armed = true;
            next.round = round;
            *self = next;
        }
    }
    pub(super) fn done(&self) -> bool { self.round == self.rounds && self.ended.is_some_and(|at| at.elapsed() >= Duration::from_secs(3)) }
    fn passed(&self) -> bool {
        let passed = if self.ring_timeout {
            self.ringing.is_some() && self.active.is_none() && self.ended.is_some() && self.missed
                && !self.end_requested && self.ring_ms.is_some_and(|ms| (58_000..=70_000).contains(&ms))
        } else { self.ringing.is_some() && self.active.is_some() && self.ended.is_some()
            && self.voice_only && self.details_hidden && self.muted && self.unmuted && self.remuted && self.end_requested
            && (!self.talk_check || self.pressed && self.released && self.saw_reopening && self.saw_talking && !self.mic_unavailable
                && !self.talk_lost_while_held && !self.input_interrupted && !self.talk_id_changed) };
        passed && (self.rounds == 1 || self.mic_on && !self.mic_unavailable)
    }
    fn report(&self, passed: bool) -> String {
        let result = format!("passed={passed}\nringing={}\nactive={}\nvoice_only={}\nvideo_details_hidden={}\nmute_selected={}\nunmute_selected={}\nremute_selected={}\nend_requested={}\nended={}\nmic_on_reported={}\nmic_unavailable_reported={}\nhold_to_talk_check={}\ntalk_pressed={}\ntalk_released={}\nmic_reopening_observed={}\ntalking_after_ready={}\n",
            self.ringing.is_some(), self.active.is_some(), self.voice_only, self.details_hidden,
            self.muted, self.unmuted, self.remuted, self.end_requested, self.ended.is_some(), self.mic_on, self.mic_unavailable,
            self.talk_check, self.pressed, self.released, self.saw_reopening, self.saw_talking);
        let result = format!("{result}talk_ready_ms={:?}\ntalk_lost_while_held={}\ninput_interrupted={}\ntalk_id_changed={}\n", self.talk_ready_ms, self.talk_lost_while_held, self.input_interrupted, self.talk_id_changed);
        format!("{result}ring_timeout_check={}\nmissed_call={}\nring_duration_ms={:?}\nround={}\nrounds_requested={}\npc_form_check={}\n", self.ring_timeout, self.missed, self.ring_ms, self.round, self.rounds, std::env::var("MESHLIVE_PHONE_UI_PC_FORM").as_deref() == Ok("1"))
    }
    pub(super) fn finish(&self) -> Result<(), String> {
        let passed = self.round == self.rounds && self.passed();
        let result = self.report(passed);
        std::fs::create_dir_all(&self.dir).map_err(|_| "Result directory")?;
        if self.rounds == 2 { std::fs::write(self.dir.join(format!("call-ui-round-{}.txt", self.round)), &result).map_err(|_| "Round result write")?; }
        std::fs::write(self.dir.join("call-ui-result.txt"), result).map_err(|_| "Result write")?;
        if passed { Ok(()) } else { Err("Native window call flow did not complete".into()) }
    }
}
