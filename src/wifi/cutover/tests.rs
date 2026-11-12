use super::sim::{
    Cfg, CutoverSim, Explored, code, explore_cutover, kind, replay_cutover, shrink_cutover, a, b,
    CLOSE_BY, CONSUME, DELIVER, DETECT, DROP, DUPLICATE, FAULT, KEYS, NEW_ATTEMPT, NEW_UP, QUALITY,
    QUIESCE, RENEWAL_FAILED, REPLACE_STANDBY, STANDBY_EVENT, STANDBY_STALE, STATE_CHANGE,
    SWITCH_OFF, TICK,
};
use super::*;
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::Arc,
};

const N: i32 = 1;

fn rec(seq: i64) -> Rec {
    Rec {
        seq,
        bytes: 100,
        tag: 0,
    }
}

fn up_machine(config: Config) -> CarrierCutover {
    let mut m = CarrierCutover::new(config);
    m.on_call_active(0, 0, true);
    m.on_carrier_pending(N, 0);
    m.on_carrier_up(N, 0, 0, true);
    m
}

fn switched(config: Config) -> CarrierCutover {
    let mut m = up_machine(config);
    m.on_tick(3_000, 0, true);
    m
}

fn writes(actions: &[Action]) -> Vec<&Action> {
    actions
        .iter()
        .filter(|action| matches!(action, Action::Write { .. }))
        .collect()
}
fn delivered(actions: &[Action]) -> Vec<i64> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Deliver { rec, .. } => Some(rec.seq),
            _ => None,
        })
        .collect()
}
fn drops(actions: &[Action]) -> Vec<DropReason> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Drop { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect()
}
fn closes(actions: &[Action]) -> Vec<(i32, CloseReason)> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::CloseCarrier { carrier, reason } => Some((*carrier, *reason)),
            _ => None,
        })
        .collect()
}
fn ended(actions: &[Action]) -> bool {
    actions
        .iter()
        .any(|action| matches!(action, Action::EndSession { .. }))
}
fn switch_generation(action: &Action) -> Option<i64> {
    match action {
        Action::Write {
            wire: Wire::Switch { generation, .. },
            ..
        } => Some(*generation),
        _ => None,
    }
}
fn resume_of(action: &Action) -> Option<(i64, i64, i32)> {
    match action {
        Action::Write {
            wire: Wire::Resume {
                generation,
                next,
                carrier,
            },
            ..
        } => Some((*generation, *next, *carrier)),
        _ => None,
    }
}

// ---------------------------------------------------------------- CarrierCutoverTest

#[test]
fn sender_switches_only_after_the_call_was_active_for_three_seconds() {
    let mut m = up_machine(Config::default());
    assert!(writes(&m.on_tick(2_999, 0, true)).is_empty());
    let out = m.on_tick(3_000, 0, true);
    let write = writes(&out)[0];
    let Action::Write { carrier, .. } = write else {
        unreachable!()
    };
    assert_eq!(*carrier, OLD);
    assert_eq!(switch_generation(write), Some(1));
    assert!(out.contains(&Action::ReassertState));
    assert_eq!(m.send_on(), N);
    assert_eq!(
        match &m.on_sealed(rec(0), 3_000)[0] {
            Action::Write { carrier, .. } => *carrier,
            _ => panic!(),
        },
        N
    );
}

#[test]
fn early_new_carrier_records_are_held_until_the_switch_marker_arrives_in_order_on_the_old_link() {
    let mut m = up_machine(Config::default());
    assert!(delivered(&m.on_arrive(N, Wire::Content(rec(5)), 10)).is_empty());
    assert_eq!(
        delivered(&m.on_arrive(OLD, Wire::Switch {
            generation: 1,
            boundary: 5,
            carrier: N
        }, 20)),
        vec![5]
    );
    assert_eq!(m.accept(), N);
    assert_eq!(
        delivered(&m.on_arrive(N, Wire::Content(rec(6)), 30)),
        vec![6]
    );
}

#[test]
fn old_tail_below_the_boundary_is_opened_and_records_at_or_above_it_are_fenced() {
    let mut m = up_machine(Config::default());
    m.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 5,
        carrier: N,
    }, 0);
    let late = m.on_arrive(OLD, Wire::Content(rec(7)), 0);
    assert_eq!(drops(&late), vec![DropReason::Fenced]);
    assert_eq!(
        delivered(&m.on_arrive(OLD, Wire::Content(rec(3)), 0)),
        vec![3]
    );
}

#[test]
fn a_stale_or_repeated_record_is_dropped_never_delivered() {
    let mut m = up_machine(Config::default());
    assert_eq!(
        delivered(&m.on_arrive(OLD, Wire::Content(rec(4)), 0)),
        vec![4]
    );
    assert!(delivered(&m.on_arrive(OLD, Wire::Content(rec(4)), 0)).is_empty());
    assert!(delivered(&m.on_arrive(OLD, Wire::Content(rec(2)), 0)).is_empty());
}

#[test]
fn a_stuck_switch_promotes_the_new_carrier_after_the_hold_and_closes_nothing() {
    let mut m = up_machine(Config::default());
    m.on_arrive(N, Wire::Content(rec(5)), 100);
    m.on_arrive(N, Wire::Content(rec(6)), 200);
    let out = m.on_tick(1_100, 0, true);
    assert_eq!(delivered(&out), vec![5, 6]);
    assert!(closes(&out).is_empty());
    assert_eq!(m.accept(), N);
    assert!(delivered(&m.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 5,
        carrier: N
    }, 2_000))
    .is_empty());
}

#[test]
fn the_first_draft_closed_the_new_carrier_on_hold_expiry_even_when_old_was_silently_dead() {
    let mut m = up_machine(Config::default().as_first_draft());
    m.on_arrive(N, Wire::Content(rec(5)), 100);
    let out = m.on_tick(1_100, 0, true);
    assert!(closes(&out)
        .iter()
        .any(|(_, reason)| *reason == CloseReason::HoldExpired));
    assert!(delivered(&out).is_empty());
}

#[test]
fn overflow_of_the_hold_promotes_like_expiry() {
    let mut m = up_machine(Config {
        hold_bytes: 250,
        ..Config::default()
    });
    assert!(delivered(&m.on_arrive(N, Wire::Content(rec(1)), 0)).is_empty());
    assert!(delivered(&m.on_arrive(N, Wire::Content(rec(2)), 0)).is_empty());
    assert_eq!(
        delivered(&m.on_arrive(N, Wire::Content(rec(3)), 0)),
        vec![1, 2, 3]
    );
}

#[test]
fn loss_of_the_old_link_moves_both_directions_to_the_new_one_at_once() {
    let mut m = up_machine(Config::default());
    let out = m.on_carrier_died(OLD, 500);
    assert_eq!(m.send_on(), N);
    assert_eq!(m.accept(), N);
    let write = writes(&out)[0];
    let Action::Write { carrier, .. } = write else {
        unreachable!()
    };
    assert_eq!(*carrier, N);
    assert!(out.contains(&Action::ReassertState));
    assert_eq!(
        delivered(&m.on_arrive(N, Wire::Content(rec(9)), 600)),
        vec![9]
    );
    assert!(ended(&m.on_carrier_died(N, 700)));
}

#[test]
fn a_repeated_switch_on_the_new_carrier_after_an_old_link_loss_is_idempotent() {
    let mut m = up_machine(Config::default());
    m.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 5,
        carrier: N,
    }, 0);
    m.on_arrive(N, Wire::Content(rec(5)), 0);
    assert!(delivered(&m.on_arrive(N, Wire::Switch {
        generation: 1,
        boundary: 5,
        carrier: N
    }, 0))
    .is_empty());
    assert_eq!(
        delivered(&m.on_arrive(N, Wire::Content(rec(6)), 0)),
        vec![6]
    );
}

#[test]
fn new_carrier_failure_falls_back_with_resume_before_any_content_and_reasserts_state() {
    let mut m = up_machine(Config::default());
    m.on_tick(3_000, 0, true);
    assert_eq!(m.send_on(), N);
    m.on_sealed(rec(0), 3_001);
    let out = m.on_carrier_died(N, 3_500);
    assert_eq!(m.send_on(), OLD);
    let resume = writes(&out)[0];
    let Action::Write { carrier, .. } = resume else {
        unreachable!()
    };
    assert_eq!(*carrier, OLD);
    assert_eq!(resume_of(resume).map(|r| r.1), Some(1));
    assert!(out.contains(&Action::ReassertState));
    assert!(out.contains(&Action::ForceLocalKeyframe));
    assert!(out.contains(&Action::ResetSendFragments));
    m.on_carrier_pending(2, 4_000);
    assert!(!writes(&m.on_carrier_up(2, 4_100, 0, true))
        .iter()
        .any(|write| switch_generation(write).is_some()));
    assert!(writes(&m.on_tick(33_000, 0, true)).is_empty());
    assert!(writes(&m.on_tick(33_600, 0, true))
        .iter()
        .any(|write| switch_generation(write).is_some()));
}

#[test]
fn resume_while_accepting_the_new_carrier_returns_to_old_and_closes_it() {
    let mut m = up_machine(Config::default());
    m.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 0,
        carrier: N,
    }, 0);
    let out = m.on_arrive(OLD, Wire::Resume {
        generation: 2,
        next: 3,
        carrier: N,
    }, 10);
    assert_eq!(m.accept(), OLD);
    assert!(!closes(&out).is_empty());
    assert!(delivered(&m.on_arrive(N, Wire::Content(rec(1)), 20)).is_empty());
}

#[test]
fn resume_naming_the_new_carrier_closes_it_even_while_still_accepting_old() {
    let mut m = up_machine(Config::default());
    let out = m.on_arrive(OLD, Wire::Resume {
        generation: 1,
        next: 3,
        carrier: N,
    }, 0);
    assert!(closes(&out)
        .iter()
        .any(|(carrier, _)| *carrier == N));
    assert_eq!(m.accept(), OLD);
    assert!(delivered(&m.on_arrive(N, Wire::Content(rec(9)), 5)).is_empty());
}

#[test]
fn the_first_draft_ignored_resume_while_accepting_old() {
    let mut m = up_machine(Config {
        resume_always_written: false,
        ..Config::default()
    });
    assert!(closes(&m.on_arrive(OLD, Wire::Resume {
        generation: 1,
        next: 3,
        carrier: N
    }, 0))
    .is_empty());
}

#[test]
fn resume_naming_another_carrier_is_ignored() {
    let mut m = up_machine(Config::default());
    assert!(closes(&m.on_arrive(OLD, Wire::Resume {
        generation: 1,
        next: 3,
        carrier: 7
    }, 0))
    .is_empty());
}

#[test]
fn a_new_carrier_dying_while_old_lives_always_tells_the_peer_even_before_any_switch() {
    let mut m = up_machine(Config::default());
    let out = m.on_carrier_died(N, 100);
    let resume = writes(&out)[0];
    let Action::Write { carrier, .. } = resume else {
        unreachable!()
    };
    assert_eq!(*carrier, OLD);
    assert_eq!(resume_of(resume).map(|r| r.2), Some(N));
    assert_eq!(m.send_on(), OLD);
    assert!(!out.contains(&Action::ReassertState));
    let mut draft = up_machine(Config {
        resume_always_written: false,
        ..Config::default()
    });
    assert!(writes(&draft.on_carrier_died(N, 100)).is_empty());
}

#[test]
fn resume_next_becomes_the_floor_of_what_the_old_link_may_deliver_afterwards() {
    let mut m = up_machine(Config::default());
    m.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 0,
        carrier: N,
    }, 0);
    m.on_arrive(OLD, Wire::Resume {
        generation: 2,
        next: 7,
        carrier: N,
    }, 10);
    assert_eq!(m.accept(), OLD);
    let low = m.on_arrive(OLD, Wire::Content(rec(5)), 20);
    assert!(delivered(&low).is_empty());
    assert_eq!(
        delivered(&m.on_arrive(OLD, Wire::Content(rec(7)), 30)),
        vec![7]
    );
    let mut draft = up_machine(Config {
        resume_sets_old_floor: false,
        ..Config::default()
    });
    draft.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 0,
        carrier: N,
    }, 0);
    draft.on_arrive(OLD, Wire::Resume {
        generation: 2,
        next: 7,
        carrier: N,
    }, 10);
    assert_eq!(
        delivered(&draft.on_arrive(OLD, Wire::Content(rec(5)), 20)),
        vec![5]
    );
}

#[test]
fn a_marker_that_names_no_carrier_is_burned_when_markers_must_name_one() {
    let mut m = up_machine(Config::default());
    m.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 5,
        carrier: ANY,
    }, 0);
    assert_eq!(m.accept(), OLD);
    let out = m.on_arrive(OLD, Wire::Resume {
        generation: 2,
        next: 7,
        carrier: ANY,
    }, 10);
    assert!(closes(&out).is_empty());
    assert_eq!(
        delivered(&m.on_arrive(OLD, Wire::Content(rec(1)), 20)),
        vec![1]
    );
}

#[test]
fn a_switch_on_the_new_carrier_is_counted_and_never_closes_the_old_link() {
    let mut m = up_machine(Config::default());
    let stats = Rc::new(RefCell::new(Vec::new()));
    let sink = stats.clone();
    m.on_stat = Some(Box::new(move |key| sink.borrow_mut().push(key.to_string())));
    let out = m.on_arrive(N, Wire::Switch {
        generation: 1,
        boundary: 4,
        carrier: N,
    }, 50);
    assert!(closes(&out).is_empty());
    assert!(stats.borrow().iter().any(|key| key == "old-dead-hint"));
    assert!(m.old_is_alive());
    assert_eq!(m.accept(), N);
    assert_eq!(m.send_on(), OLD);
    assert!(!writes(&out)
        .iter()
        .any(|write| switch_generation(write).is_some()));
    assert!(closes(&m.on_arrive(N, Wire::Switch {
        generation: 1,
        boundary: 4,
        carrier: N
    }, 60))
    .is_empty());
    let mut off = up_machine(Config {
        old_dead_hint: false,
        ..Config::default()
    });
    let off_stats = Rc::new(RefCell::new(Vec::new()));
    let sink = off_stats.clone();
    off.on_stat = Some(Box::new(move |key| sink.borrow_mut().push(key.to_string())));
    assert!(closes(&off.on_arrive(N, Wire::Switch {
        generation: 1,
        boundary: 4,
        carrier: N
    }, 50))
    .is_empty());
    assert!(!off_stats.borrow().iter().any(|key| key == "old-dead-hint"));
}

#[test]
fn switch_waits_behind_an_old_link_backlog_and_for_a_frame_boundary() {
    let mut m = up_machine(Config::default());
    assert!(writes(&m.on_tick(3_000, 500, true)).is_empty());
    assert!(writes(&m.on_tick(3_100, 0, false)).is_empty());
    let out = m.on_tick(3_200, 499, true);
    let write = writes(&out)[0];
    let Action::Write {
        carrier, priority, ..
    } = write
    else {
        unreachable!()
    };
    assert_eq!(*carrier, OLD);
    assert!(!*priority);
    let mut draft = up_machine(Config {
        switch_through_media_fifo: false,
        gate_switch_on_old_queue: false,
        ..Config::default()
    });
    let out = draft.on_tick(3_000, 5_000, true);
    assert!(match writes(&out)[0] {
        Action::Write { priority, .. } => *priority,
        _ => false,
    });
}

#[test]
fn receiver_asks_the_peer_for_keyframes_and_the_sender_forces_its_own() {
    let mut m = up_machine(Config::default());
    m.on_tick(3_000, 0, true);
    let fallback = m.on_carrier_died(N, 3_100);
    assert!(fallback.contains(&Action::ForceLocalKeyframe));
    assert!(fallback.contains(&Action::ResetSendFragments));
    assert!(!fallback.contains(&Action::RequestPeerKeyframe));
    let gap = up_machine(Config::default()).on_carrier_died(OLD, 100);
    assert!(gap.contains(&Action::RequestPeerKeyframe));
    assert!(gap.contains(&Action::ResetSendFragments));
    assert!(!gap.contains(&Action::ForceLocalKeyframe));
}

#[test]
fn losing_the_old_link_while_the_new_one_is_still_being_set_up_waits_for_it() {
    let mut m = CarrierCutover::new(Config::default());
    m.on_call_active(0, 0, true);
    m.on_carrier_pending(N, 0);
    let out = m.on_carrier_died(OLD, 100);
    assert!(!ended(&out));
    let up = m.on_carrier_up(N, 200, 0, true);
    assert_eq!(m.send_on(), N);
    assert!(up.iter().any(|action| matches!(
        action,
        Action::Write {
            carrier,
            wire: Wire::Switch { .. },
            ..
        } if *carrier == N
    )));
    let mut draft = CarrierCutover::new(Config::default().as_first_draft());
    draft.on_carrier_pending(N, 0);
    assert!(ended(&draft.on_carrier_died(OLD, 100)));
}

#[test]
fn a_switch_marker_that_arrives_before_this_sides_ready_is_kept() {
    let mut m = CarrierCutover::new(Config::default());
    m.on_call_active(0, 0, true);
    m.on_carrier_pending(N, 0);
    m.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 4,
        carrier: N,
    }, 10);
    m.on_carrier_up(N, 20, 0, true);
    assert_eq!(m.accept(), N);
    assert_eq!(
        delivered(&m.on_arrive(N, Wire::Content(rec(4)), 30)),
        vec![4]
    );
}

fn stale_markers_meet_a_second_attempt(config: Config) -> (CarrierCutover, Vec<Action>) {
    let mut m = CarrierCutover::new(config);
    m.on_call_active(0, 0, true);
    m.on_carrier_pending(1, 0);
    m.on_carrier_up(1, 0, 0, true);
    m.on_carrier_died(1, 100);
    m.on_carrier_pending(2, 200);
    m.on_carrier_up(2, 300, 0, true);
    let named = if config.markers_name_carrier { 1 } else { ANY };
    m.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 5,
        carrier: named,
    }, 400);
    let out = m.on_arrive(OLD, Wire::Resume {
        generation: 2,
        next: 7,
        carrier: named,
    }, 410);
    (m, out)
}

#[test]
fn markers_of_a_dead_attempt_do_not_move_the_receiver_onto_a_later_attempt() {
    let (m, out) = stale_markers_meet_a_second_attempt(Config::default());
    assert_eq!(m.accept(), OLD);
    assert!(closes(&out).is_empty());
}

#[test]
fn the_first_draft_applied_a_stale_switch_to_the_later_attempt_and_then_closed_it_on_the_stale_resume() {
    let config = Config {
        markers_name_carrier: false,
        ..Config::default()
    };
    let mut m = CarrierCutover::new(config);
    m.on_call_active(0, 0, true);
    m.on_carrier_pending(1, 0);
    m.on_carrier_up(1, 0, 0, true);
    m.on_carrier_died(1, 100);
    m.on_carrier_pending(2, 200);
    m.on_carrier_up(2, 300, 0, true);
    m.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 5,
        carrier: ANY,
    }, 400);
    assert_eq!(m.accept(), 2);
    let fenced = m.on_arrive(OLD, Wire::Content(rec(6)), 405);
    assert_eq!(drops(&fenced), vec![DropReason::Fenced]);
    assert!(!closes(&m.on_arrive(OLD, Wire::Resume {
        generation: 2,
        next: 7,
        carrier: ANY
    }, 410))
    .is_empty());
}

#[test]
fn upgrade_attempt_that_never_becomes_ready_is_closed_at_its_ceiling() {
    let mut m = CarrierCutover::new(Config::default());
    m.on_carrier_pending(N, 0);
    assert!(m.on_tick(4_999, 0, true).is_empty());
    assert!(!closes(&m.on_tick(5_000, 0, true)).is_empty());
}

// ---------------------------------------------------------------- CarrierCutoverEventsTest

#[test]
fn a_restored_standby_keeps_the_call_on_wifi_rewrites_the_switch_and_becomes_the_fallback() {
    let mut m = switched(Config::default());
    m.on_carrier_died(OLD, 3_100);
    assert!(!m.old_is_alive());
    assert_eq!(m.send_on(), N);
    assert_eq!(m.accept(), N);
    let out = m.on_standby_replaced(4_000);
    assert!(m.old_is_alive());
    assert_eq!(m.send_on(), N);
    assert_eq!(m.accept(), N);
    assert!(!ended(&out));
    let write = writes(&out)[0];
    let Action::Write { carrier, .. } = write else {
        unreachable!()
    };
    assert_eq!(*carrier, OLD);
    assert_eq!(switch_generation(write), Some(1));
    let after = m.on_carrier_died(N, 5_000);
    assert!(!ended(&after));
    assert!(!m.ended());
    assert_eq!(m.send_on(), OLD);
    assert_eq!(m.accept(), OLD);
    assert_eq!(m.fallback_count(), 1);
    assert_eq!(
        match &write {
            Action::Write {
                wire: Wire::Switch {
                    generation,
                    boundary,
                    carrier: named,
                },
                ..
            } => (*generation, *boundary, *named),
            _ => panic!(),
        },
        (1, 0, N)
    );
}

#[test]
fn the_first_draft_never_hears_of_the_renewal_and_ends_the_call_when_wifi_dies() {
    let mut m = switched(Config {
        standby_replaced_event: false,
        ..Config::default()
    });
    m.on_carrier_died(OLD, 3_100);
    assert!(m.on_standby_replaced(4_000).is_empty());
    assert!(!m.old_is_alive());
    let after = m.on_carrier_died(N, 5_000);
    assert!(ended(&after));
    assert!(m.ended());
}

#[test]
fn a_standby_replacement_always_reasserts_state() {
    let mut moving = switched(Config::default());
    let moved = moving.on_standby_replaced(4_000);
    assert_eq!(
        moved
            .iter()
            .filter(|action| *action == &Action::ReassertState)
            .count(),
        1
    );
    let marker = writes(&moved)[0];
    assert!(
        moved.iter().position(|action| action == &Action::ReassertState)
            > moved.iter().position(|action| action == marker)
    );
    let mut idle = up_machine(Config::default());
    let idle_out = idle.on_standby_replaced(4_000);
    assert_eq!(
        idle_out
            .iter()
            .filter(|action| *action == &Action::ReassertState)
            .count(),
        1
    );
}

#[test]
fn a_fallback_that_happened_is_announced_again_on_the_new_standby() {
    let mut m = switched(Config::default());
    m.on_carrier_died(N, 4_000);
    assert_eq!(m.send_on(), OLD);
    let out = m.on_standby_replaced(5_000);
    let write = writes(&out)[0];
    let Action::Write { carrier, .. } = write else {
        unreachable!()
    };
    assert_eq!(*carrier, OLD);
    assert_eq!(resume_of(write), Some((2, 0, N)));
    assert_eq!(m.send_on(), OLD);
}

#[test]
fn a_renewal_with_nothing_to_announce_writes_nothing() {
    let mut m = CarrierCutover::new(Config::default());
    m.on_call_active(0, 0, true);
    let out = m.on_standby_replaced(10);
    assert!(writes(&out).is_empty());
    assert_eq!(
        out.iter()
            .filter(|action| *action == &Action::ReassertState)
            .count(),
        1
    );
    assert_eq!(m.send_on(), OLD);
    assert_eq!(m.accept(), OLD);
    assert!(writes(&up_machine(Config::default()).on_standby_replaced(10)).is_empty());
}

#[test]
fn the_first_draft_does_not_repeat_the_marker() {
    let mut m = switched(Config {
        reassert_marker_after_replace: false,
        ..Config::default()
    });
    m.on_carrier_died(OLD, 3_100);
    let out = m.on_standby_replaced(4_000);
    assert!(m.old_is_alive());
    assert!(writes(&out).is_empty());
}

#[test]
fn a_repeated_or_stale_marker_on_the_standby_never_changes_what_the_receiver_accepts() {
    let mut rx = up_machine(Config::default());
    rx.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 5,
        carrier: N,
    }, 10);
    assert_eq!(rx.accept(), N);
    let again = rx.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 5,
        carrier: N,
    }, 20);
    assert!(again.is_empty());
    assert_eq!(rx.accept(), N);
    rx.on_arrive(OLD, Wire::Resume {
        generation: 2,
        next: 3,
        carrier: N,
    }, 30);
    assert_eq!(rx.accept(), OLD);
    let stale_switch = rx.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 5,
        carrier: N,
    }, 40);
    assert!(stale_switch
        .iter()
        .all(|action| matches!(action, Action::Drop { .. })));
    assert_eq!(rx.accept(), OLD);
    let stale_resume = rx.on_arrive(OLD, Wire::Resume {
        generation: 2,
        next: 3,
        carrier: N,
    }, 50);
    assert!(stale_resume
        .iter()
        .all(|action| matches!(action, Action::Drop { .. })));
    assert_eq!(rx.accept(), OLD);
    assert_eq!(rx.generation_seen(), 2);
}

#[test]
fn a_renewal_after_the_old_link_was_lost_while_an_attempt_was_pending_returns_sending_to_the_standby() {
    let mut m = CarrierCutover::new(Config::default());
    m.on_call_active(0, 0, true);
    m.on_carrier_pending(N, 0);
    m.on_carrier_died(OLD, 100);
    assert_eq!(m.send_on(), NONE);
    assert_eq!(m.accept(), NONE);
    let out = m.on_standby_replaced(200);
    assert_eq!(m.send_on(), OLD);
    assert_eq!(m.accept(), OLD);
    for action in [
        Action::ResetSendFragments,
        Action::ForceLocalKeyframe,
        Action::ReassertState,
        Action::SuppressFragments,
        Action::RequestPeerKeyframe,
    ] {
        assert!(out.contains(&action), "{action:?}");
    }
    assert_eq!(
        match &m.on_sealed(rec(0), 300)[0] {
            Action::Write { carrier, .. } => *carrier,
            _ => panic!(),
        },
        OLD
    );
    m.on_tick(5_000, 0, true);
    assert!(!m.ended());
    assert_eq!(m.send_on(), OLD);
}

#[test]
fn an_ended_machine_ignores_every_new_event() {
    let mut m = CarrierCutover::new(Config::default());
    m.on_call_active(0, 0, true);
    assert!(ended(&m.on_carrier_died(OLD, 10)));
    assert!(m.on_standby_replaced(20).is_empty());
    assert!(m.on_switch_off(30).is_empty());
    assert!(m.on_quality_fallback(40).is_empty());
    assert!(m.on_standby_freshness(false, 50).is_empty());
    assert!(m.ended());
}

#[test]
fn a_resume_naming_the_carrier_still_being_read_is_honoured() {
    let mut m = CarrierCutover::new(Config::default());
    m.on_call_active(0, 0, true);
    m.on_carrier_pending(N, 0);
    m.on_carrier_up(N, 0, 0, true);
    m.on_arrive(OLD, Wire::Switch {
        generation: 1,
        boundary: 5,
        carrier: N,
    }, 10);
    assert_eq!(m.accept(), N);
    m.on_carrier_pending(2, 20);
    m.on_carrier_up(2, 30, 0, true);
    m.on_carrier_died(2, 40);
    assert_eq!(m.new_carrier(), NONE);
    assert_eq!(m.accept(), N);
    let out = m.on_arrive(OLD, Wire::Resume {
        generation: 2,
        next: 7,
        carrier: N,
    }, 50);
    assert_eq!(m.accept(), OLD);
    assert_eq!(closes(&out), vec![(N, CloseReason::PeerResumed)]);
    assert!(m
        .on_arrive(OLD, Wire::Content(rec(6)), 60)
        .iter()
        .all(|action| matches!(action, Action::Drop { .. })));
    assert_eq!(
        delivered(&m.on_arrive(OLD, Wire::Content(rec(7)), 70)),
        vec![7]
    );
}

#[test]
fn a_delayed_switch_arriving_after_a_renewal_never_closes_the_new_standby() {
    let mut m = up_machine(Config::default());
    m.on_carrier_died(OLD, 100);
    m.on_standby_replaced(200);
    assert!(m.old_is_alive());
    let late = m.on_arrive(N, Wire::Switch {
        generation: 1,
        boundary: 0,
        carrier: N,
    }, 300);
    assert!(closes(&late).is_empty());
    assert!(m.old_is_alive());
    let after = m.on_carrier_died(N, 400);
    assert!(!ended(&after));
    assert!(!m.ended());
    assert_eq!(m.send_on(), OLD);
    assert_eq!(m.accept(), OLD);
}

#[test]
fn switching_wifi_off_while_the_standby_is_stale_waits_and_completes_when_it_is_heard_again() {
    let mut m = switched(Config::default());
    assert!(m.on_standby_freshness(false, 3_100).is_empty());
    assert!(m.on_switch_off(3_200).is_empty());
    assert!(m.switch_off_pending_now());
    assert_eq!(m.send_on(), N);
    assert_eq!(m.new_carrier(), N);
    let back = m.on_standby_freshness(true, 4_000);
    assert_eq!(resume_of(writes(&back)[0]), Some((2, 0, N)));
    assert_eq!(closes(&back), vec![(N, CloseReason::SwitchOff)]);
    assert_eq!(m.send_on(), OLD);
    assert!(!m.switch_off_pending_now());
    assert_eq!(m.fallback_count(), 0);
}

#[test]
fn a_wifi_death_while_a_switch_off_waits_for_a_stale_standby_falls_back_and_clears_the_wait() {
    let mut m = switched(Config::default());
    m.on_standby_freshness(false, 3_100);
    m.on_switch_off(3_200);
    let after = m.on_carrier_died(N, 3_300);
    assert!(!ended(&after));
    assert_eq!(m.send_on(), OLD);
    assert!(!m.switch_off_pending_now());
    assert!(closes(&m.on_standby_freshness(true, 4_000)).is_empty());
}

#[test]
fn a_quality_fallback_with_a_stale_standby_does_nothing() {
    let mut m = switched(Config::default());
    m.on_standby_freshness(false, 3_100);
    assert!(m.on_quality_fallback(3_200).is_empty());
    assert_eq!(m.new_carrier(), N);
    assert_eq!(m.send_on(), N);
    assert_eq!(m.fallback_count(), 0);
}

#[test]
fn the_first_draft_ignores_standby_freshness() {
    let mut m = switched(Config {
        standby_freshness_gates: false,
        ..Config::default()
    });
    m.on_standby_freshness(false, 3_100);
    let out = m.on_switch_off(3_200);
    assert_eq!(closes(&out), vec![(N, CloseReason::SwitchOff)]);
    assert!(!m.switch_off_pending_now());
}

#[test]
fn a_renewed_standby_is_fresh() {
    let mut m = switched(Config::default());
    m.on_carrier_died(OLD, 3_100);
    m.on_standby_freshness(false, 3_150);
    m.on_standby_replaced(4_000);
    assert!(m.standby_fresh_now());
}

#[test]
fn switching_wifi_off_with_a_standby_falls_back_without_counting_or_cooling_down_then_closes_the_carrier() {
    let mut m = switched(Config::default());
    let out = m.on_switch_off(3_500);
    let write = writes(&out)[0];
    let Action::Write { carrier, .. } = write else {
        unreachable!()
    };
    assert_eq!(*carrier, OLD);
    assert_eq!(resume_of(write), Some((2, 0, N)));
    for action in [
        Action::ResetSendFragments,
        Action::ForceLocalKeyframe,
        Action::ReassertState,
    ] {
        assert!(out.contains(&action), "{action:?}");
    }
    assert_eq!(closes(&out), vec![(N, CloseReason::SwitchOff)]);
    let position_write = out.iter().position(|action| action == write).unwrap();
    let position_close = out
        .iter()
        .position(|action| matches!(action, Action::CloseCarrier { .. }))
        .unwrap();
    assert!(position_close > position_write);
    assert!(matches!(out.last(), Some(Action::CloseCarrier { .. })));
    assert_eq!(m.send_on(), OLD);
    assert_eq!(m.accept(), OLD);
    assert_eq!(m.new_carrier(), NONE);
    assert_eq!(m.fallback_count(), 0);
    assert!(m.switched_off_now());
    assert!(!m.switch_off_pending_now());
}

#[test]
fn after_the_switch_is_off_no_attempt_is_accepted_and_nothing_switches_again() {
    let mut m = switched(Config::default());
    m.on_switch_off(3_500);
    assert!(m.on_carrier_pending(2, 3_600).is_empty());
    assert_eq!(m.pending_carrier(), NONE);
    assert!(m.on_carrier_up(2, 3_700, 0, true).is_empty());
    assert_eq!(m.new_carrier(), NONE);
    assert!(writes(&m.on_tick(60_000, 0, true)).is_empty());
    assert!(m.on_switch_off(70_000).is_empty());
}

#[test]
fn the_first_draft_keeps_accepting_attempts_after_the_switch_is_off() {
    let mut m = CarrierCutover::new(Config {
        switch_off_blocks_attempts: false,
        ..Config::default()
    });
    m.on_call_active(0, 0, true);
    m.on_switch_off(1);
    m.on_carrier_pending(N, 2);
    assert_eq!(m.pending_carrier(), N);
}

#[test]
fn switching_wifi_off_while_wifi_is_the_only_carrier_keeps_the_call_and_completes_when_a_standby_is_back() {
    let mut m = switched(Config::default());
    m.on_carrier_died(OLD, 3_100);
    let out = m.on_switch_off(3_200);
    assert!(out.is_empty());
    assert_eq!(m.send_on(), N);
    assert!(m.switched_off_now());
    assert!(m.switch_off_pending_now());
    assert!(!m.ended());
    let back = m.on_standby_replaced(4_000);
    assert_eq!(resume_of(writes(&back)[0]), Some((2, 0, N)));
    assert_eq!(closes(&back), vec![(N, CloseReason::SwitchOff)]);
    assert!(!back.iter().any(|action| matches!(
        action,
        Action::Write {
            wire: Wire::Switch { .. },
            ..
        }
    )));
    assert_eq!(m.send_on(), OLD);
    assert_eq!(m.accept(), OLD);
    assert!(!m.switch_off_pending_now());
    assert_eq!(m.fallback_count(), 0);
    assert!(!m.ended());
}

#[test]
fn if_wifi_dies_before_a_standby_is_back_the_session_ends_only_then() {
    let mut m = switched(Config::default());
    m.on_carrier_died(OLD, 3_100);
    m.on_switch_off(3_200);
    assert!(ended(&m.on_carrier_died(N, 3_300)));
}

#[test]
fn the_first_draft_closes_the_only_live_carrier_when_the_switch_is_turned_off() {
    let mut m = switched(Config {
        switch_off_keeps_last_carrier: false,
        ..Config::default()
    });
    m.on_carrier_died(OLD, 3_100);
    let out = m.on_switch_off(3_200);
    assert!(ended(&out));
    assert!(m.ended());
}

#[test]
fn switching_wifi_off_cancels_an_attempt_in_progress_when_a_standby_is_alive() {
    let mut m = CarrierCutover::new(Config::default());
    m.on_call_active(0, 0, true);
    m.on_carrier_pending(N, 0);
    let out = m.on_switch_off(100);
    assert_eq!(closes(&out), vec![(N, CloseReason::SwitchOff)]);
    assert_eq!(m.pending_carrier(), NONE);
    assert!(!m.ended());
    assert_eq!(m.send_on(), OLD);
}

#[test]
fn switching_wifi_off_while_an_attempt_is_the_only_hope_keeps_the_attempt() {
    let mut m = CarrierCutover::new(Config::default());
    m.on_call_active(0, 0, true);
    m.on_carrier_pending(N, 0);
    m.on_carrier_died(OLD, 100);
    let out = m.on_switch_off(200);
    assert!(out.is_empty());
    assert_eq!(m.pending_carrier(), N);
    assert!(m.switch_off_pending_now());
    let up = m.on_carrier_up(N, 300, 0, true);
    assert!(!ended(&up));
    assert_eq!(m.send_on(), N);
    let back = m.on_standby_replaced(400);
    assert!(closes(&back)
        .iter()
        .any(|(carrier, reason)| *carrier == N && *reason == CloseReason::SwitchOff));
    assert_eq!(m.send_on(), OLD);
    assert!(!m.ended());
}

#[test]
fn switching_off_before_anything_happened_only_blocks_future_attempts() {
    let mut m = CarrierCutover::new(Config::default());
    m.on_call_active(0, 0, true);
    assert!(m.on_switch_off(0).is_empty());
    assert!(m.on_carrier_pending(N, 1).is_empty());
    assert_eq!(m.pending_carrier(), NONE);
    assert!(!m.ended());
}

#[test]
fn switching_off_while_the_new_carrier_is_up_but_this_direction_never_moved_still_releases_it() {
    let mut m = up_machine(Config::default());
    let out = m.on_switch_off(1_000);
    assert_eq!(closes(&out), vec![(N, CloseReason::SwitchOff)]);
    assert_eq!(m.new_carrier(), NONE);
    assert_eq!(m.fallback_count(), 0);
    assert_eq!(m.send_on(), OLD);
    assert!(matches!(
        writes(&out)[0],
        Action::Write {
            wire: Wire::Resume { .. },
            ..
        }
    ));
}

#[test]
fn a_quality_fallback_is_a_counted_planned_fallback_with_cooldown_that_eventually_stops_wifi() {
    let mut m = switched(Config::default());
    let out = m.on_quality_fallback(4_000);
    let write = writes(&out)[0];
    assert_eq!(resume_of(write).map(|r| r.0), Some(2));
    let Action::Write { carrier, .. } = write else {
        unreachable!()
    };
    assert_eq!(*carrier, OLD);
    assert_eq!(closes(&out), vec![(N, CloseReason::Quality)]);
    assert!(
        out.iter()
            .position(|action| matches!(action, Action::CloseCarrier { .. }))
            > out.iter().position(|action| action == write)
    );
    assert_eq!(m.fallback_count(), 1);
    assert_eq!(m.send_on(), OLD);
    m.on_carrier_pending(2, 4_100);
    m.on_carrier_up(2, 4_200, 0, true);
    assert!(writes(&m.on_tick(33_999, 0, true)).is_empty());
    let again = m.on_tick(34_000, 0, true);
    let marker = writes(&again)[0];
    assert_eq!(switch_generation(marker), Some(3));
    assert_eq!(
        match marker {
            Action::Write {
                wire: Wire::Switch { carrier, .. },
                ..
            } => *carrier,
            _ => -99,
        },
        2
    );
    m.on_quality_fallback(40_000);
    assert_eq!(m.fallback_count(), 2);
    m.on_carrier_pending(3, 41_000);
    m.on_carrier_up(3, 41_100, 0, true);
    assert!(writes(&m.on_tick(200_000, 0, true)).is_empty());
}

#[test]
fn a_quality_fallback_counts_even_when_this_direction_never_moved_but_not_in_the_first_draft() {
    let mut counted = up_machine(Config::default());
    let out = counted.on_quality_fallback(1_000);
    assert_eq!(counted.fallback_count(), 1);
    assert_eq!(closes(&out), vec![(N, CloseReason::Quality)]);
    assert!(matches!(
        writes(&out)[0],
        Action::Write {
            wire: Wire::Resume { .. },
            ..
        }
    ));
    let mut draft = up_machine(Config {
        quality_fallback_counts: false,
        ..Config::default()
    });
    draft.on_quality_fallback(1_000);
    assert_eq!(draft.fallback_count(), 0);
}

#[test]
fn a_quality_fallback_with_no_standby_does_nothing() {
    let mut m = switched(Config::default());
    m.on_carrier_died(OLD, 3_100);
    assert!(m.on_quality_fallback(3_200).is_empty());
    assert_eq!(m.new_carrier(), N);
    assert_eq!(m.send_on(), N);
    assert!(!m.ended());
}

// ---------------------------------------------------------------- CarrierCutoverModelTest

fn small_machine() -> Config {
    Config {
        hold_ms: 1_000,
        hold_bytes: 3,
        active_before_switch_ms: 1_000,
        cooldown_ms: 2_000,
        max_fallbacks: 2,
        pending_max_ms: 2_000,
        ..Config::default()
    }
}

fn small_machine_with(hold_expiry: HoldExpiry, pending_keeps_session: bool) -> Config {
    Config {
        hold_expiry,
        pending_keeps_session,
        ..small_machine()
    }
}

fn exhaustive_cfg(horizon: i32, media: i32) -> Cfg {
    let machine = small_machine();
    Cfg {
        tick_ms: 1_000,
        machine,
        horizon_ticks: horizon,
        quiet_ticks: 9,
        detect_max_ticks: 2,
        media_per_endpoint: media,
        media_bytes: 1,
        state_changes_per_endpoint: 0,
        fault_budget: 1,
        max_attempts: 1,
        keys: 1,
        max_delay_ticks: 2,
        quiesce_anywhere: false,
        close_by: false,
        attempt_by_tick: 1,
        fault_by_tick: horizon - 1,
        up_immediately: true,
        auto_quiesce: true,
        max_consume_lag_ticks: 0,
        ..Cfg::new(1_000, machine, horizon, 9, 2, media, 1, 0, 1, 1)
    }
}

fn format_trace(cfg: Cfg, codes: &[i32]) -> String {
    let result = replay_cutover(cfg, codes, true).expect("replay");
    let log = result.log.expect("trace");
    log.into_iter()
        .filter(|line| {
            !line.ends_with(" tick")
                && !(line.trim_start().starts_with("A: write rec#")
                    || line.trim_start().starts_with("B: write rec#"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn check_explored(
    name: &str,
    cfg: Cfg,
    must_reach: &[&str],
    budget: u64,
    allow_capped: bool,
) -> Explored {
    let started = std::time::Instant::now();
    let result = explore_cutover(cfg.clone(), budget);
    println!(
        "T-CX-M exhaustive {name}: states={} terminals={} ms={}{}",
        result.states,
        result.terminals,
        started.elapsed().as_millis(),
        if result.capped { " capped=true" } else { "" }
    );
    println!(
        "  coverage of {name}: {}",
        result
            .coverage
            .iter()
            .collect::<std::collections::BTreeMap<_, _>>()
            .into_iter()
            .map(|(key, count)| format!("{key}={count}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    for key in must_reach {
        assert!(
            result.coverage.get(*key).copied().unwrap_or(0) > 0,
            "{name} never reached '{key}'"
        );
    }
    if let Some(violation) = &result.violation {
        let minimum = shrink_cutover(
            cfg.clone(),
            result.trace.as_deref().unwrap_or(&[]),
            violation.split(' ').next().unwrap_or(""),
        );
        println!(
            "VIOLATION in {name}: {violation}\n{}",
            format_trace(cfg, &minimum)
        );
    }
    assert!(result.violation.is_none(), "{name}: {:?}", result.violation);
    if !allow_capped {
        assert!(
            !result.capped,
            "{name}: state budget exhausted before the search finished ({} states)",
            result.states
        );
    }
    result
}

#[test]
fn exhaustive_switch_hold_and_fallback_with_one_fault() {
    check_explored(
        "switch/hold/fallback, 1 fault",
        exhaustive_cfg(6, 3),
        &[
            "write-switch-on-old",
            "write-switch-on-new",
            "write-resume-on-old",
            "write-switch-on-old-into-dead",
            "switch-accepted-from-old",
            "old-dead-hint",
            "new-died-resume-notice",
            "resume-closed-new-not-accepted",
            "switch-already-accepted",
            "hold-exhausted",
            "old-died-new-left",
            "new-died-fell-back",
            "resume-closed-new",
            "resume-burned",
            "end-session",
            "drop-fenced",
            "drop-stale",
            "reassert",
        ],
        6_000_000,
        false,
    );
}

#[test]
fn exhaustive_with_the_reader_queue_separate_from_the_consumer() {
    let mut cfg = exhaustive_cfg(4, 1);
    cfg.reader_queue = true;
    check_explored(
        "reader queue, 1 fault",
        cfg,
        &[
            "drop-closed_carrier",
            "write-switch-on-new",
            "hold-exhausted",
            "new-died-fell-back",
        ],
        6_000_000,
        false,
    );
}

#[test]
fn exhaustive_with_two_faults() {
    let mut cfg = exhaustive_cfg(4, 1);
    cfg.fault_budget = 2;
    check_explored(
        "2 faults (both carriers, either order)",
        cfg,
        &[],
        6_000_000,
        false,
    );
}

#[test]
fn exhaustive_while_the_new_carrier_is_still_being_set_up() {
    let mut cfg = exhaustive_cfg(5, 1);
    cfg.up_immediately = false;
    check_explored(
        "handshake pending, READY at any time",
        cfg,
        &[
            "switch-kept-for-pending",
            "switch-applied-at-ready",
            "old-died-waiting-for-pending",
            "close-handshake_timeout",
        ],
        6_000_000,
        false,
    );
}

#[test]
fn exhaustive_with_a_second_attempt_after_the_first_died() {
    let mut cfg = exhaustive_cfg(7, 1);
    cfg.max_attempts = 2;
    check_explored(
        "two attempts",
        cfg,
        &["switch-burned", "write-switch-on-old", "new-died-fell-back"],
        6_000_000,
        false,
    );
}

#[test]
fn exhaustive_with_indispensable_state_changes() {
    let mut cfg = exhaustive_cfg(3, 1);
    cfg.state_changes_per_endpoint = 1;
    check_explored("state changes at any time", cfg, &[], 6_000_000, false);
}

#[test]
fn exhaustive_with_duplicated_records() {
    let mut cfg = exhaustive_cfg(3, 1);
    cfg.duplicates = true;
    check_explored("duplicated records", cfg, &[], 6_000_000, false);
}

#[test]
fn exhaustive_with_dropped_records() {
    let mut cfg = exhaustive_cfg(3, 1);
    cfg.drops = true;
    check_explored("dropped records (safety only)", cfg, &[], 6_000_000, false);
}

fn lossless_machine() -> Config {
    Config {
        hold_ms: 2_000,
        hold_bytes: 100,
        active_before_switch_ms: 2_000,
        ..small_machine()
    }
}

#[test]
fn exhaustive_normal_switch_with_an_old_link_backlog_loses_nothing() {
    for frame_every in [1, 2] {
        let mut cfg = exhaustive_cfg(6, 4);
        cfg.machine = lossless_machine();
        cfg.fault_budget = 0;
        cfg.max_delay_ticks = 1;
        cfg.require_lossless = true;
        cfg.frame_every = frame_every;
        let mut must = vec![
            "write-switch-on-old",
            "switch-accepted-from-old",
            "switch-waits-for-old-queue",
        ];
        if frame_every > 1 {
            must.push("switch-waits-for-frame");
        }
        check_explored(
            &format!("lossless normal switch, frames of {frame_every}"),
            cfg,
            &must,
            6_000_000,
            false,
        );
    }
    let mut cfg = exhaustive_cfg(6, 4);
    cfg.machine = lossless_machine();
    cfg.machine.gate_switch_on_old_queue = false;
    cfg.fault_budget = 0;
    cfg.max_delay_ticks = 1;
    cfg.require_lossless = true;
    check_explored(
        "lossless normal switch, FIFO without the gate",
        cfg,
        &["write-switch-on-old", "switch-accepted-from-old"],
        6_000_000,
        false,
    );
}

#[test]
fn exhaustive_normal_switch_with_the_reader_queue_loses_nothing() {
    let mut cfg = exhaustive_cfg(5, 3);
    cfg.machine = lossless_machine();
    cfg.fault_budget = 0;
    cfg.max_delay_ticks = 1;
    cfg.require_lossless = true;
    cfg.reader_queue = true;
    check_explored(
        "lossless normal switch, reader queue",
        cfg,
        &["write-switch-on-old", "switch-accepted-from-old"],
        6_000_000,
        false,
    );
}

#[test]
fn exhaustive_renewal_replaces_the_dead_old_link_while_wifi_lives() {
    let mut cfg = exhaustive_cfg(2, 1);
    cfg.fault_budget = 2;
    cfg.replacements = 1;
    check_explored(
        "renewal after the old link died",
        cfg,
        &[
            "standby-replaced",
            "reassert-switch",
            "old-died-new-left",
            "new-died-fell-back",
        ],
        6_000_000,
        false,
    );
}

#[test]
fn exhaustive_renewal_with_the_reader_queue() {
    let mut cfg = exhaustive_cfg(2, 1);
    cfg.fault_budget = 2;
    cfg.replacements = 1;
    cfg.reader_queue = true;
    check_explored(
        "renewal, reader queue",
        cfg,
        &["standby-replaced", "heldDeaths", "droppedHeldDeaths"],
        30_000_000,
        true,
    );
}

#[test]
fn exhaustive_switch_off_at_any_time_by_one_phone() {
    let mut cfg = exhaustive_cfg(6, 2);
    cfg.switch_offs = 1;
    check_explored(
        "switch-off by one phone",
        cfg,
        &[
            "switch-off",
            "switch-off-fell-back",
            "pending-ignored-after-switch-off",
            "write-resume-on-old",
        ],
        6_000_000,
        false,
    );
}

#[test]
fn exhaustive_switch_off_by_both_phones() {
    let mut cfg = exhaustive_cfg(5, 1);
    cfg.fault_budget = 0;
    cfg.switch_offs = 2;
    check_explored(
        "switch-off by both phones",
        cfg,
        &["switch-off", "switch-off-fell-back"],
        6_000_000,
        false,
    );
}

#[test]
fn exhaustive_switch_off_while_wifi_is_the_only_carrier_completes_when_the_standby_is_back() {
    let mut cfg = exhaustive_cfg(2, 1);
    cfg.switch_offs = 1;
    cfg.replacements = 1;
    cfg.stale_max_ticks = 0;
    check_explored(
        "switch-off with Wi-Fi only, then renewal",
        cfg,
        &[
            "switch-off-wifi-only",
            "switch-off-standby-stale",
            "switch-off-completed-on-replace",
        ],
        6_000_000,
        false,
    );
}

#[test]
fn exhaustive_quality_fallback_at_any_time() {
    let mut cfg = exhaustive_cfg(7, 1);
    cfg.quality_fallbacks = 2;
    check_explored(
        "quality fallback",
        cfg,
        &["quality-fallback"],
        6_000_000,
        false,
    );
}

#[test]
fn exhaustive_delayed_switch_after_renewal_never_closes_the_standby() {
    let mut cfg = exhaustive_cfg(2, 1);
    cfg.fault_budget = 2;
    cfg.replacements = 1;
    cfg.max_delay_ticks = 2;
    check_explored(
        "delayed SWITCH after a renewal",
        cfg,
        &["standby-replaced", "old-dead-hint", "new-died-fell-back"],
        6_000_000,
        false,
    );
}

#[test]
fn exhaustive_switch_off_and_quality_with_a_stale_standby() {
    let mut cfg = exhaustive_cfg(3, 1);
    cfg.switch_offs = 1;
    cfg.quality_fallbacks = 1;
    cfg.stale_max_ticks = 0;
    check_explored(
        "stale standby",
        cfg,
        &["standby-stale", "switch-off-standby-stale"],
        6_000_000,
        false,
    );
}

// ---------------------------------------------------------------- seeded random runs

fn random_cfg(duplicates: bool, drops: bool, reader_queue: bool) -> Cfg {
    let machine = Config {
        cooldown_ms: 4_000,
        ..Config::default()
    };
    Cfg {
        tick_ms: 100,
        machine,
        horizon_ticks: 150,
        quiet_ticks: 150,
        detect_max_ticks: 20,
        media_per_endpoint: 150,
        media_bytes: 20_000,
        state_changes_per_endpoint: 4,
        fault_budget: 3,
        max_attempts: 2,
        duplicates,
        drops,
        reader_queue,
        ..Cfg::new(100, machine, 150, 150, 20, 150, 20_000, 4, 3, 2)
    }
}

struct Rng(u64);
impl Rng {
    fn new(seed: i64) -> Self {
        Self(if seed == 0 { 0x9e37_79b9_7f4a_7c15 } else { seed as u64 })
    }
    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }
    fn int(&mut self, bound: i32) -> i32 {
        (self.next() % bound.max(1) as u64) as i32
    }
    fn boolean(&mut self) -> bool {
        self.next() & 1 == 1
    }
    fn double(&mut self) -> f64 {
        (self.next() >> 11) as f64 / ((1u64 << 53) as f64)
    }
    fn shuffle<T>(&mut self, values: &mut [T]) {
        for index in (1..values.len()).rev() {
            let other = self.int((index + 1) as i32) as usize;
            values.swap(index, other);
        }
    }
}

fn tail_lines(log: &[String]) -> String {
    let filtered: Vec<&String> = log.iter().filter(|line| !line.ends_with(" tick")).collect();
    filtered
        .iter()
        .rev()
        .take(400)
        .rev()
        .map(|line| (*line).clone())
        .collect::<Vec<String>>()
        .join("\n")
}

struct Tally {
    runs: usize,
    switched: usize,
    resumed: usize,
    promoted: usize,
    ended: usize,
    with_faults: usize,
    stale_drops: u64,
    void_writes: u64,
    keyframes: u64,
}
impl std::fmt::Display for Tally {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "runs={} switched={} resumed={} holdExhaustions={} sessionsEndedLegitimately={} runsWithFault={} staleDrops={} writesIntoDeadCarrier={} keyframeRequests={}",
            self.runs,
            self.switched,
            self.resumed,
            self.promoted,
            self.ended,
            self.with_faults,
            self.stale_drops,
            self.void_writes,
            self.keyframes
        )
    }
}

fn apply_if_enabled(sim: &mut CutoverSim, action: i32) -> bool {
    if !sim.is_enabled(action) {
        return false;
    }
    sim.apply(action);
    true
}

fn run_seed(seed: i64, cfg: &Cfg, trace: bool, record: bool, variable_bytes: bool) -> CutoverSim {
    let mut rng = Rng::new(seed);
    let mut sim_cfg = cfg.clone();
    if variable_bytes {
        sim_cfg.media_bytes_of = Some(Arc::new(move |x: usize, seq: i64| {
            let h = (seed
                .wrapping_mul(1_000_003)
                .wrapping_add(x as i64 * 7_919)
                .wrapping_add(seq))
            .wrapping_mul(-7_046_029_254_386_353_131);
            let r = ((h as u64) >> 33) as i32 & 0x7fff_ffff;
            if r % 100 < 3 {
                (1_000_000 + r % 2_000_000) as i64
            } else {
                (1_000 + r % 40_000) as i64
            }
        }));
    }
    let mut sim = CutoverSim::new(sim_cfg);
    if trace {
        sim.log = Some(Vec::new());
    }
    if record {
        sim.record = Some(Vec::new());
    }
    let attempt_tick = if rng.int(10) < 9 { rng.int(60) } else { -1 };
    let attempt2_tick = if attempt_tick >= 0 {
        attempt_tick + 10 + rng.int(80)
    } else {
        -1
    };
    let fault_count = match rng.int(100) {
        0..=24 => 0,
        25..=59 => 1,
        60..=84 => 2,
        _ => 3,
    };
    let mut faults = Vec::new();
    for _ in 0..fault_count {
        let tick = 10 + rng.int(cfg.horizon_ticks - 40);
        let on_new = rng.boolean();
        let by = if rng.int(10) < 6 { -1 } else { rng.int(2) };
        faults.push((tick, on_new, by));
    }
    let mut changes = Vec::new();
    for x in 0..2i32 {
        let count = rng.int(cfg.state_changes_per_endpoint + 1);
        for _ in 0..count {
            changes.push((rng.int(cfg.horizon_ticks - 5), x, rng.int(KEYS as i32)));
        }
    }
    let slots = 1 + cfg.max_attempts;
    let mut stall_from = vec![vec![-1i32; slots as usize]; 2];
    let mut stall_to = vec![vec![-1i32; slots as usize]; 2];
    let mut p_deliver = vec![vec![0.0; slots as usize]; 2];
    for side in 0..2 {
        for carrier in 0..slots as usize {
            p_deliver[side][carrier] = [0.25, 0.6, 1.0][rng.int(3) as usize];
        }
    }
    for side in 0..2 {
        for carrier in 0..slots as usize {
            if rng.int(100) < 30 {
                stall_from[side][carrier] = rng.int(cfg.horizon_ticks - 30);
                stall_to[side][carrier] = stall_from[side][carrier] + 1 + rng.int(30);
            }
        }
    }
    let mut up_at = [vec![i32::MAX; slots as usize], vec![i32::MAX; slots as usize]];
    let mut detect_at = [vec![i32::MAX; slots as usize], vec![i32::MAX; slots as usize]];
    let extra = cfg.replacements + cfg.switch_offs + cfg.quality_fallbacks > 0;
    let mut ext = Rng::new(seed ^ 0x5de_ece6);
    let replace_ticks: Vec<i32> = if extra {
        (0..cfg.replacements)
            .map(|_| 20 + ext.int(cfg.horizon_ticks - 60))
            .collect()
    } else {
        Vec::new()
    };
    let switch_off_tick = if extra && cfg.switch_offs > 0 && ext.int(100) < 50 {
        10 + ext.int(cfg.horizon_ticks - 40)
    } else {
        -1
    };
    let switch_off_by = if extra { ext.int(2) } else { 0 };
    let quality_ticks: Vec<i32> = if extra {
        (0..cfg.quality_fallbacks)
            .map(|_| 15 + ext.int(cfg.horizon_ticks - 50))
            .collect()
    } else {
        Vec::new()
    };
    let quality_by = if extra { ext.int(2) } else { 0 };
    fn up_delay(rng: &mut Rng) -> i32 {
        match rng.int(10) {
            0..=6 => 1 + rng.int(10),
            7..=8 => 10 + rng.int(60),
            _ => 400,
        }
    }
    fn detect_delay(rng: &mut Rng, max: i32) -> i32 {
        match rng.int(10) {
            0..=2 => 0,
            3..=5 => max,
            _ => rng.int(max + 1),
        }
    }
    let mut guard = 0;
    let mut buffer = Vec::new();
    let mut shuffler = Rng::new(seed);
    while sim.violation.is_none() && guard < 2_000_000 {
        guard += 1;
        let tick = sim.tick;
        if !sim.phase2 {
            if tick == attempt_tick
                || (tick >= attempt2_tick && attempt2_tick >= 0 && sim.attempts_started() == 1)
            {
                if apply_if_enabled(&mut sim, code(NEW_ATTEMPT, 0, 0)) {
                    let attempt = sim.attempts_started() as usize;
                    for x in 0..2 {
                        up_at[x][attempt] = tick + up_delay(&mut rng);
                    }
                }
            }
            for x in 0..2 {
                let attempt = sim.attempts_started() as usize;
                if attempt > 0 && tick >= up_at[x][attempt] {
                    apply_if_enabled(&mut sim, code(NEW_UP, x as i32, 0));
                }
            }
            for (at, on_new, by) in &faults {
                if *at == tick {
                    let carrier = if *on_new { sim.attempts_started() } else { 0 };
                    let applied = if *by < 0 {
                        apply_if_enabled(&mut sim, code(FAULT, carrier, 0))
                    } else {
                        apply_if_enabled(&mut sim, code(CLOSE_BY, *by, carrier))
                    };
                    if applied {
                        for x in 0..2 {
                            detect_at[x][carrier as usize] =
                                tick + detect_delay(&mut rng, cfg.detect_max_ticks);
                        }
                    }
                }
            }
            for (at, x, key) in &changes {
                if *at == tick {
                    apply_if_enabled(&mut sim, code(STATE_CHANGE, *x, *key));
                }
            }
            for at in &replace_ticks {
                if *at == tick {
                    apply_if_enabled(&mut sim, code(REPLACE_STANDBY, 0, 0));
                }
            }
            if tick == switch_off_tick {
                apply_if_enabled(&mut sim, code(SWITCH_OFF, switch_off_by, 0));
            }
            for at in &quality_ticks {
                if *at == tick {
                    apply_if_enabled(&mut sim, code(QUALITY, quality_by, 0));
                }
            }
        }
        let mut order: Vec<usize> = (0..(2 * slots) as usize).collect();
        shuffler.shuffle(&mut order);
        for position in order {
            let from = position % 2;
            let carrier = position / 2;
            let stalled = !sim.phase2
                && tick >= stall_from[from][carrier]
                && tick < stall_to[from][carrier];
            while !stalled
                && sim.can_deliver(from, carrier)
                && (sim.phase2 || rng.double() < p_deliver[from][carrier])
            {
                let roll = rng.int(100);
                if cfg.duplicates && roll < 4 {
                    sim.apply(code(DUPLICATE, from as i32, carrier as i32));
                } else if cfg.drops && (4..=5).contains(&roll) {
                    sim.apply(code(DROP, from as i32, carrier as i32));
                } else {
                    sim.apply(code(DELIVER, from as i32, carrier as i32));
                }
                if sim.violation.is_some() {
                    break;
                }
            }
        }
        for x in 0..2 {
            while sim.violation.is_none()
                && sim.can_consume(x)
                && (sim.phase2 || rng.double() < 0.6)
            {
                sim.apply(code(CONSUME, x as i32, 0));
            }
        }
        for x in 0..2 {
            if sim.is_enabled(code(STANDBY_EVENT, x as i32, 0))
                && (sim.phase2 || ext.double() < 0.5)
            {
                sim.apply(code(STANDBY_EVENT, x as i32, 0));
            }
        }
        if cfg.stale_max_ticks >= 0 {
            for x in 0..2 {
                if sim.is_enabled(code(STANDBY_STALE, x as i32, 0))
                    && (sim.phase2 || ext.double() < 0.2)
                {
                    sim.apply(code(STANDBY_STALE, x as i32, 0));
                }
            }
        }
        for x in 0..2 {
            if sim.is_enabled(code(RENEWAL_FAILED, x as i32, 0))
                && (sim.phase2 || ext.double() < 0.5)
            {
                sim.apply(code(RENEWAL_FAILED, x as i32, 0));
            }
        }
        sim.enabled(&mut buffer);
        for action in buffer.clone() {
            if kind(action) == DETECT && tick >= detect_at[a(action) as usize][b(action) as usize] {
                apply_if_enabled(&mut sim, action);
            }
        }
        sim.enabled(&mut buffer);
        if buffer.contains(&code(TICK, 0, 0)) {
            sim.apply(code(TICK, 0, 0));
            if !sim.phase2 && sim.tick >= cfg.horizon_ticks {
                sim.apply(code(QUIESCE, 0, 0));
            }
        } else {
            let forced = buffer.iter().copied().find(|action| {
                let k = kind(*action);
                k == DETECT
                    || k == DELIVER
                    || k == CONSUME
                    || k == NEW_UP
                    || k == STANDBY_EVENT
                    || k == STANDBY_STALE
                    || k == RENEWAL_FAILED
            });
            if let Some(action) = forced {
                sim.apply(action);
            } else if buffer.contains(&code(QUIESCE, 0, 0)) {
                sim.apply(code(QUIESCE, 0, 0));
            } else {
                break;
            }
        }
    }
    sim.check_final();
    sim
}

fn random_seed_range(name: &str, seeds: std::ops::RangeInclusive<i64>, cfg: Cfg) {
    let mut tally = Tally {
        runs: 0,
        switched: 0,
        resumed: 0,
        promoted: 0,
        ended: 0,
        with_faults: 0,
        stale_drops: 0,
        void_writes: 0,
        keyframes: 0,
    };
    let started = std::time::Instant::now();
    for seed in seeds.clone() {
        let sim = run_seed(seed, &cfg, false, false, true);
        tally.runs += 1;
        if sim.switch_writes > 0 {
            tally.switched += 1;
        }
        if sim.resume_writes > 0 {
            tally.resumed += 1;
        }
        if sim.m.iter().any(|machine| machine.hold_exhaustions() > 0) {
            tally.promoted += 1;
        }
        if sim.ended.iter().any(|ended| *ended) {
            tally.ended += 1;
        }
        if sim.void_writes > 0 {
            tally.with_faults += 1;
        }
        tally.stale_drops += sim.dropped_stale;
        tally.void_writes += sim.void_writes;
        tally.keyframes += sim.keyframes;
        if let Some(violation) = &sim.violation {
            let again = run_seed(seed, &cfg, true, false, true);
            let lines = again.log.as_ref().map(|log| tail_lines(log)).unwrap_or_default();
            println!("RANDOM {name} seed {seed}: {violation}\n{lines}");
            panic!("{name} seed {seed}: {violation}");
        }
    }
    println!(
        "random {name} seeds {}..{}: {tally} ms={}",
        seeds.start(),
        seeds.end(),
        started.elapsed().as_millis()
    );
}

#[test]
fn random_seeds() {
    random_seed_range("contract", 1..=20_000, random_cfg(false, false, true));
}

#[test]
fn random_seeds_with_duplicated_records() {
    random_seed_range(
        "duplicates",
        100_001..=105_000,
        random_cfg(true, false, true),
    );
}

#[test]
fn random_seeds_with_dropped_records() {
    random_seed_range("drops", 200_001..=205_000, random_cfg(false, true, true));
}

fn renewal_cfg() -> Cfg {
    let mut cfg = random_cfg(false, false, true);
    cfg.replacements = 2;
    cfg.switch_offs = 1;
    cfg.quality_fallbacks = 2;
    cfg.stale_max_ticks = 10;
    cfg
}

#[test]
fn random_seeds_with_renewal_switch_off_and_quality_fallback() {
    let mut replaced = 0;
    let mut switched_off = 0;
    let mut quality = 0;
    let seeds = 300_001..=310_000;
    let started = std::time::Instant::now();
    for seed in seeds.clone() {
        let sim = run_seed(seed, &renewal_cfg(), false, false, true);
        if sim.replacements_used > 0 {
            replaced += 1;
        }
        if sim.switch_offs_used > 0 {
            switched_off += 1;
        }
        if sim.quality_used > 0 {
            quality += 1;
        }
        if let Some(violation) = &sim.violation {
            let again = run_seed(seed, &renewal_cfg(), true, false, true);
            let lines = again.log.as_ref().map(|log| tail_lines(log)).unwrap_or_default();
            println!("RANDOM renewal seed {seed}: {violation}\n{lines}");
            panic!("renewal seed {seed}: {violation}");
        }
    }
    println!(
        "random renewal seeds {}..{}: replaced={replaced} switchedOff={switched_off} quality={quality} ms={}",
        seeds.start(),
        seeds.end(),
        started.elapsed().as_millis()
    );
    assert!(replaced > 0, "no run replaced a standby");
    assert!(switched_off > 0, "no run switched Wi-Fi off");
    assert!(quality > 0, "no run fired the quality rule");
}

fn first_draft_violation(name: &str, cfg: Cfg) -> (String, Vec<i32>) {
    let result = explore_cutover(cfg.clone(), 6_000_000);
    let violation = result
        .violation
        .clone()
        .unwrap_or_else(|| panic!("{name}: the model did not find the known defect"));
    let minimum = shrink_cutover(
        cfg.clone(),
        result.trace.as_deref().unwrap_or(&[]),
        violation.split(' ').next().unwrap_or(""),
    );
    println!(
        "first-draft counterexample [{name}] after {} states: {violation} (shrunk from {} to {} steps)\n{}",
        result.states,
        result.trace.as_ref().map_or(0, |trace| trace.len()),
        minimum.len(),
        format_trace(cfg, &minimum)
    );
    (violation, minimum)
}

#[test]
fn first_draft_hold_expiry_closing_the_new_carrier_is_caught() {
    let machine = small_machine_with(HoldExpiry::CloseNew, true);
    let mut cfg = exhaustive_cfg(5, 3);
    cfg.machine = machine;
    let (violation, trace) = first_draft_violation("hold expiry closes N", cfg);
    assert!(violation.starts_with("1 "), "{violation}");
    assert!(trace.iter().any(|action| kind(*action) == FAULT));
}

#[test]
fn first_draft_switch_overtaking_the_old_link_backlog_is_caught() {
    let mut machine = lossless_machine();
    machine.switch_through_media_fifo = false;
    machine.gate_switch_on_old_queue = false;
    let mut cfg = exhaustive_cfg(6, 4);
    cfg.machine = machine;
    cfg.fault_budget = 0;
    cfg.max_delay_ticks = 1;
    cfg.require_lossless = true;
    let (violation, _) = first_draft_violation("SWITCH overtakes the old link's backlog", cfg);
    assert!(violation.starts_with("8 "), "{violation}");
}

#[test]
fn first_draft_ending_the_session_while_the_new_carrier_is_being_set_up_is_caught() {
    let machine = small_machine_with(HoldExpiry::Promote, false);
    let mut cfg = exhaustive_cfg(4, 1);
    cfg.machine = machine;
    cfg.up_immediately = false;
    let (violation, _) = first_draft_violation("O lost during the N handshake", cfg);
    assert!(violation.starts_with("1 "), "{violation}");
}

#[test]
fn first_draft_switch_off_closing_the_only_carrier_is_caught() {
    let mut machine = small_machine();
    machine.switch_off_keeps_last_carrier = false;
    let mut cfg = exhaustive_cfg(6, 1);
    cfg.machine = machine;
    cfg.switch_offs = 1;
    let (violation, trace) = first_draft_violation("switch-off closes the last carrier", cfg);
    assert!(violation.starts_with("1 "), "{violation}");
    assert!(trace.iter().any(|action| kind(*action) == SWITCH_OFF));
}

#[test]
fn first_draft_accepting_attempts_after_the_switch_off_is_caught() {
    let mut machine = small_machine();
    machine.switch_off_blocks_attempts = false;
    let mut cfg = exhaustive_cfg(4, 1);
    cfg.machine = machine;
    cfg.fault_budget = 0;
    cfg.switch_offs = 1;
    let (violation, _) = first_draft_violation("attempt after the switch-off", cfg);
    assert!(violation.starts_with("9 "), "{violation}");
}

#[test]
fn first_draft_switch_off_ignoring_a_stale_standby_is_caught() {
    let mut machine = small_machine();
    machine.standby_freshness_gates = false;
    let mut cfg = exhaustive_cfg(6, 1);
    cfg.machine = machine;
    cfg.switch_offs = 1;
    cfg.stale_max_ticks = 0;
    let (violation, trace) = first_draft_violation("switch-off with a stale standby", cfg);
    assert!(violation.starts_with("12 "), "{violation}");
    assert!(trace.iter().any(|action| kind(*action) == SWITCH_OFF));
}

#[test]
fn first_draft_markers_without_a_carrier_name_mislead_a_later_attempt_and_are_caught() {
    let mut cfg = random_cfg(false, false, true);
    cfg.machine = Config {
        cooldown_ms: 4_000,
        markers_name_carrier: false,
        ..Config::default()
    };
    let seeds = std::iter::once(27_889i64).chain(1..=100_000);
    for seed in seeds {
        let sim = run_seed(seed, &cfg, false, true, false);
        let Some(violation) = sim.violation else {
            continue;
        };
        let minimum = shrink_cutover(
            cfg.clone(),
            sim.record.as_deref().unwrap_or(&[]),
            violation.split(' ').next().unwrap_or(""),
        );
        println!(
            "first-draft counterexample [SWITCH/RESUME name no carrier] from random seed {seed}: {violation} (shrunk from {} to {} steps)\n{}",
            sim.record.as_ref().map_or(0, |record| record.len()),
            minimum.len(),
            format_trace(cfg.clone(), &minimum)
        );
        assert!(
            violation.starts_with("4 ") || violation.starts_with("5 "),
            "{violation}"
        );
        assert!(
            minimum.iter().filter(|action| kind(**action) == NEW_ATTEMPT).count() >= 2
        );
        return;
    }
    panic!("no seed found the known defect");
}

// ---------------------------------------------------------------- the half-open case, measured

struct HalfOpen {
    scenario: String,
    sender: char,
    detect_ticks: i32,
    gap_ms: i64,
    sealed: usize,
    opened: usize,
}

fn half_open_run(
    machine: Config,
    kill_old: bool,
    after_switch: bool,
    d_a: i32,
    d_b: i32,
    b_switches: bool,
    lag: i32,
    tick_ms: i64,
) -> Vec<HalfOpen> {
    let machine_b = if b_switches {
        None
    } else {
        Some(Config {
            active_before_switch_ms: i64::MAX / 2,
            ..machine
        })
    };
    let cfg = Cfg {
        tick_ms,
        machine,
        horizon_ticks: 600,
        quiet_ticks: 400,
        detect_max_ticks: 40,
        media_per_endpoint: 600,
        media_bytes: 1_000,
        state_changes_per_endpoint: 0,
        fault_budget: 1,
        max_attempts: 1,
        fault_drops_in_flight: lag > 0,
        machine_b,
        ..Cfg::new(tick_ms, machine, 600, 400, 40, 600, 1_000, 0, 1, 1)
    };
    let mut sim = CutoverSim::new(cfg.clone());
    sim.apply(code(NEW_ATTEMPT, 0, 0));
    let fault_tick = if after_switch { 100 } else { 20 };
    let victim = if kill_old { 0 } else { 1 };
    let mut detect_at = [i32::MAX; 2];
    let mut buffer = Vec::new();
    let mut guard = 0;
    while sim.violation.is_none() && guard < 100_000 {
        guard += 1;
        if sim.tick == 1 {
            for x in 0..2 {
                if sim.is_enabled(code(NEW_UP, x as i32, 0)) {
                    sim.apply(code(NEW_UP, x as i32, 0));
                }
            }
        }
        if sim.tick == fault_tick
            && !sim.phase2
            && sim.is_enabled(code(FAULT, victim, 0))
        {
            sim.apply(code(FAULT, victim, 0));
            detect_at = [sim.tick + d_a, sim.tick + d_b];
        }
        for carrier in 0..2 {
            for from in 0..2 {
                while sim.can_deliver(from, carrier) && sim.head_age_ticks(from, carrier) >= lag {
                    sim.apply(code(DELIVER, from as i32, carrier as i32));
                }
            }
        }
        sim.enabled(&mut buffer);
        for action in buffer.clone() {
            if kind(action) == DETECT && sim.tick >= detect_at[a(action) as usize] {
                sim.apply(action);
            }
        }
        sim.enabled(&mut buffer);
        if buffer.contains(&code(TICK, 0, 0)) {
            sim.apply(code(TICK, 0, 0));
            if !sim.phase2 && sim.tick >= cfg.horizon_ticks {
                sim.apply(code(QUIESCE, 0, 0));
            }
        } else {
            let forced = buffer
                .iter()
                .copied()
                .find(|action| {
                    let k = kind(*action);
                    k == DETECT || k == DELIVER || k == CONSUME || k == NEW_UP
                })
                .or_else(|| buffer.contains(&code(QUIESCE, 0, 0)).then_some(code(QUIESCE, 0, 0)));
            match forced {
                Some(action) => sim.apply(action),
                None => break,
            }
        }
    }
    sim.check_final();
    assert!(
        sim.violation.is_none(),
        "old={kill_old} afterSwitch={after_switch} dA={d_a} dB={d_b} bSwitches={b_switches}: {:?}",
        sim.violation
    );
    let what = format!(
        "{}{}{}",
        if kill_old { "O" } else { "N" },
        if after_switch {
            " dies after the switch"
        } else {
            " dies before the switch"
        },
        if b_switches { "" } else { ", B stays on O" }
    );
    (0..2)
        .map(|x| {
            let stats = sim.media_stats(x);
            HalfOpen {
                scenario: what.clone(),
                sender: (b'A' + x as u8) as char,
                detect_ticks: if x == 0 { d_a } else { d_b },
                gap_ms: i64::from(stats.max_gap_ticks) * tick_ms,
                sealed: stats.sealed,
                opened: stats.opened,
            }
        })
        .collect()
}

#[test]
fn half_open_media_loss_is_bounded_by_the_senders_own_detection_delay() {
    let mut worst_by_cell: HashMap<String, i64> = HashMap::new();
    let mut worst_new = 0i64;
    let variants = [
        (
            "round 6 text",
            Config {
                old_dead_hint: false,
                resume_always_written: false,
                ..Config::default()
            },
        ),
        ("(b) RESUME always, the corrected text", Config::default()),
    ];
    let classes: [(&str, &[(i32, i32)]); 3] = [
        ("both notice at once", &[(0, 0)]),
        ("one notices at once", &[(0, 40), (40, 0)]),
        ("neither notices before 2 s", &[(40, 40)]),
    ];
    for (variant_name, machine) in variants {
        for (class_name, delays) in &classes {
            let mut worst = 0i64;
            let mut worst_lost = 0;
            for b_switches in [true, false] {
                for kill_old in [true, false] {
                    for after in [true, false] {
                        for (d_a, d_b) in delays.iter().copied() {
                            for result in half_open_run(
                                machine,
                                kill_old,
                                after,
                                d_a,
                                d_b,
                                b_switches,
                                0,
                                50,
                            ) {
                                worst = worst.max(result.gap_ms);
                                worst_lost = worst_lost.max(result.sealed - result.opened);
                                assert!(
                                    result.gap_ms <= i64::from(result.detect_ticks) * 50 + 200,
                                    "{variant_name} / {class_name} / {} {}: gap {} ms",
                                    result.scenario,
                                    result.sender,
                                    result.gap_ms
                                );
                            }
                        }
                    }
                }
            }
            worst_by_cell.insert(format!("{variant_name} / {class_name}"), worst);
            println!(
                "half-open [{variant_name}] {class_name}: worst media gap {worst} ms, worst loss {worst_lost} of 600 records per direction (20 per second)"
            );
            if variant_name.starts_with("(b) RESUME always") {
                worst_new = worst_new.max(worst);
            }
        }
    }
    println!("half-open worst media gap with the corrected text: {worst_new} ms");
    for (class_name, _) in &classes {
        assert!(
            worst_by_cell[&format!("(b) RESUME always, the corrected text / {class_name}")]
                <= worst_by_cell[&format!("round 6 text / {class_name}")]
        );
    }
    assert!(
        worst_by_cell["(b) RESUME always, the corrected text / both notice at once"] <= 300
    );
}

#[test]
fn half_open_loss_adds_the_dead_carriers_queued_backlog() {
    for lag in [0, 5] {
        let mut worst = 0i64;
        for kill_old in [true, false] {
            for after in [true, false] {
                for result in half_open_run(
                    Config::default(),
                    kill_old,
                    after,
                    40,
                    40,
                    true,
                    lag,
                    50,
                ) {
                    worst = worst.max(result.gap_ms);
                    assert!(
                        result.gap_ms <= 2_000 + i64::from(lag) * 50 + 200,
                        "lag {lag} {} {}: gap {} ms",
                        result.scenario,
                        result.sender,
                        result.gap_ms
                    );
                }
            }
        }
        println!(
            "half-open, neither notices before 2 s, carrier backlog {} ms: worst media gap {worst} ms",
            lag * 50
        );
    }
}
