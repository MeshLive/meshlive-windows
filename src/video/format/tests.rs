use super::*;
use crate::video::fixtures::*;
use std::collections::VecDeque;

fn header(codec: u8) -> VideoHeader {
    VideoHeader {
        width: 1280,
        height: 720,
        codec,
        fps: 15,
        quarter_turns: 0,
        opus: false,
    }
}
fn packet(flags: u8, pts_us: i64, payload: &[u8]) -> Packet<'_> {
    Packet {
        flags,
        pts_us,
        payload,
    }
}
fn key(tracker: &mut FormatTracker, payload: &[u8]) -> PacketPlan {
    tracker.plan(packet(KEY_FRAME, 0, payload)).unwrap()
}
fn config(tracker: &mut FormatTracker, payload: &[u8]) -> PacketPlan {
    tracker.plan(packet(CONFIG, 0, payload)).unwrap()
}
fn size(format: &VideoFormat) -> (u16, u16) {
    (format.width, format.height)
}

// Mirrors Android's codec-free DecoderFormatTrackerTest. The consumer applies
// the reset/rebuild plan before draining; it is not a native codec/session test.
struct Consumer {
    tracker: FormatTracker,
    formats: Vec<Arc<VideoFormat>>,
    queued: VecDeque<(u8, i64)>,
    fed: Vec<(usize, i64)>,
}
impl Consumer {
    fn new(codec: u8, csd: &[&[u8]], first: &[u8]) -> Self {
        let tracker = FormatTracker::new(header(codec), csd, Some(first)).unwrap();
        let formats = vec![tracker.recovery().unwrap()];
        Self {
            tracker,
            formats,
            queued: VecDeque::new(),
            fed: Vec::new(),
        }
    }
    fn offer(&mut self, flags: u8, pts: i64, bytes: &[u8]) {
        let plan = self.tracker.plan(packet(flags, pts, bytes)).unwrap();
        if !plan.queue {
            return;
        }
        if plan.reset_queue {
            self.queued.clear();
        }
        if let Some(rebuild) = plan.rebuild {
            self.formats.push(rebuild.format);
        }
        self.queued.push_back((flags, pts));
    }
    fn drain(&mut self) {
        while let Some((flags, pts)) = self.queued.pop_front() {
            if flags & CONFIG == 0 {
                self.fed.push((self.formats.len() - 1, pts));
            }
        }
    }
}

#[test]
fn unchanged_configuration_and_discontinuity_keep_decoder_but_drop_old_queued_frames() {
    let csd = android_av1_sequence(640, 360);
    let picture = [csd.clone(), frame(0, false)].concat();
    let mut consumer = Consumer::new(3, &[&csd], &picture);
    consumer.offer(KEY_FRAME, 0, &picture);
    consumer.offer(0, 66, &frame(1, false));
    assert!(config(&mut consumer.tracker, &csd).queue);
    consumer.offer(KEY_FRAME | DISCONTINUITY, 133, &picture);
    consumer.offer(DISCARDABLE, 200, &frame(1, false));
    consumer.drain();
    assert_eq!(consumer.formats.len(), 1);
    assert_eq!(consumer.fed, [(0, 133), (0, 200)]);
}
#[test]
fn av1_new_configuration_is_held_then_rebuilt_at_key_and_recovered_from_newest_csd() {
    let old = android_av1_sequence(1280, 720);
    let new = android_av1_sequence(640, 360);
    let first = [old.clone(), frame(0, false)].concat();
    let next = [new.clone(), frame(0, false)].concat();
    let mut consumer = Consumer::new(3, &[&old], &first);
    let old_snapshot = consumer.tracker.recovery().unwrap();
    consumer.offer(KEY_FRAME, 0, &first);
    consumer.offer(0, 66, &frame(1, false));
    consumer.offer(CONFIG, 0, &new);
    assert_eq!(size(consumer.tracker.current().unwrap()), (1280, 720));
    consumer.offer(0, 80, &frame(1, false));
    consumer.offer(KEY_FRAME | DISCONTINUITY, 133, &next);
    consumer.offer(0, 200, &frame(1, false));
    consumer.drain();
    assert_eq!(consumer.formats.len(), 2);
    assert_eq!(consumer.fed, [(1, 133), (1, 200)]);
    let recovery = consumer.tracker.recovery().unwrap();
    assert_eq!(size(&recovery), (640, 360));
    assert_eq!(recovery.configurations()[0].as_ref(), new);
    assert_eq!(old_snapshot.configurations()[0].as_ref(), old);
    assert!(key(&mut consumer.tracker, &next).rebuild.is_none());
    assert!(Arc::ptr_eq(
        &recovery,
        &consumer.tracker.recovery().unwrap()
    ));
}
#[test]
fn av1_raw_and_av1c_out_of_band_config_select_newest_but_in_band_size_wins() {
    let old = android_av1_sequence(1280, 720);
    let raw = android_av1_sequence(640, 360);
    let av1c = [&[0x81, 0x08, 0, 0][..], raw.as_slice()].concat();
    for newest in [&raw, &av1c] {
        let mut tracker = FormatTracker::new(header(3), &[&old], None).unwrap();
        assert!(!config(&mut tracker, newest).queue);
        let rebuild = key(&mut tracker, &frame(0, false)).rebuild.unwrap();
        assert_eq!(size(&rebuild.format), (640, 360));
        assert!(rebuild.size_changed && rebuild.configuration_changed);
        assert_eq!(
            rebuild.format.configurations()[0].as_ref(),
            newest.as_slice()
        );
    }
    let in_band = [raw, frame(0, false)].concat();
    let mut tracker = FormatTracker::new(header(3), &[&old], None).unwrap();
    assert_eq!(
        size(&key(&mut tracker, &in_band).rebuild.unwrap().format),
        (640, 360)
    );
    assert!(tracker.current().unwrap().configurations().is_empty()); // size-only: stale CSD dropped
}
#[test]
fn vp9_unmarked_size_changes_drop_old_queue_and_config_on_up_and_down_rungs() {
    let mut consumer = Consumer::new(2, &[&[9, 9, 9]], &vp9_key(640, 360));
    consumer.offer(KEY_FRAME, 0, &vp9_key(640, 360));
    consumer.offer(0, 66, &[0x86]);
    consumer.offer(KEY_FRAME, 133, &vp9_key(426, 240));
    consumer.drain();
    assert_eq!(consumer.fed, [(1, 133)]);
    assert!(
        consumer
            .tracker
            .current()
            .unwrap()
            .configurations()
            .is_empty()
    );
    consumer.offer(KEY_FRAME, 200, &vp9_key(960, 540));
    consumer.offer(KEY_FRAME, 300, &vp9_key(960, 540));
    assert_eq!(
        consumer.formats.iter().map(|p| size(p)).collect::<Vec<_>>(),
        [(640, 360), (426, 240), (960, 540)]
    );
}
#[test]
fn same_size_csd_changes_rebuild_distinct_vp9_packets_but_repeated_subset_does_not() {
    let first = vp9_key(640, 360);
    let mut tracker = FormatTracker::new(header(2), &[&[1], &[2]], Some(&first)).unwrap();
    assert!(config(&mut tracker, &[1]).queue);
    assert!(key(&mut tracker, &first).rebuild.is_none());
    assert_eq!(tracker.current().unwrap().configurations().len(), 2);
    assert!(!config(&mut tracker, &[5]).queue);
    assert!(!config(&mut tracker, &[6]).queue);
    assert!(!config(&mut tracker, &[6]).queue);
    let rebuild = key(&mut tracker, &first).rebuild.unwrap();
    assert!(!rebuild.size_changed && rebuild.configuration_changed);
    assert_eq!(
        rebuild
            .format
            .configurations()
            .iter()
            .map(|p| p.as_ref())
            .collect::<Vec<_>>(),
        [&[5][..], &[6][..]]
    );
}
#[test]
fn av1_initial_selection_uses_actual_last_packet_and_held_selection_matches_android_dedup() {
    let a = android_av1_sequence(640, 360);
    let b = android_av1_sequence(1280, 720);
    let mut tracker = FormatTracker::new(header(3), &[&a, &b, &a], Some(&frame(0, false))).unwrap();
    assert_eq!(size(tracker.current().unwrap()), (640, 360));
    assert_eq!(tracker.current().unwrap().configurations()[0].as_ref(), a);
    config(&mut tracker, &a);
    config(&mut tracker, &b);
    config(&mut tracker, &a);
    let rebuild = key(&mut tracker, &frame(0, false)).rebuild.unwrap();
    assert_eq!(size(&rebuild.format), (1280, 720));
    assert_eq!(rebuild.format.configurations().len(), 1);
    assert_eq!(rebuild.format.configurations()[0].as_ref(), b);
}
#[test]
fn unreadable_size_leaves_format_unchanged_without_bypassing_native_preflight() {
    for codec in [2, 3] {
        let mut tracker = FormatTracker::new(header(codec), &[], None).unwrap();
        let old = tracker.recovery().unwrap();
        assert!(key(&mut tracker, &[1, 2, 3, 4]).rebuild.is_none());
        assert!(Arc::ptr_eq(&old, &tracker.recovery().unwrap()));
        assert!(
            super::super::bitstream::SizeGuard::new(header(codec))
                .inspect(&[1, 2, 3, 4])
                .is_err()
        );
    }
}

#[test]
fn initial_av1_size_hint_fallback_does_not_replace_selected_decoder_csd() {
    let valid = android_av1_sequence(640, 360);
    let unreadable = [0x81, 0x08, 0, 0];
    let tracker =
        FormatTracker::new(header(3), &[&valid, &unreadable], Some(&frame(0, false))).unwrap();
    assert_eq!(size(tracker.current().unwrap()), (640, 360));
    assert_eq!(
        tracker.current().unwrap().configurations()[0].as_ref(),
        unreadable
    );
    // A format hint is not permission to feed a sequence-less picture into
    // the native decoder. It still fails allocation admission.
    assert!(
        super::super::bitstream::SizeGuard::new(header(3))
            .inspect(&frame(0, false))
            .is_err()
    );
}
#[test]
fn initial_key_uses_coded_size_below_maximum_without_an_immediate_rebuild() {
    let first = vp9_key(426, 240);
    let mut tracker = FormatTracker::new(header(2), &[], Some(&first)).unwrap();
    assert_eq!(size(tracker.current().unwrap()), (426, 240));
    assert!(key(&mut tracker, &first).rebuild.is_none());
    assert!(matches!(
        FormatTracker::new(header(2), &[], Some(&vp9_key(65536, 360))),
        Err(FormatError::Size)
    ));
    let mut invalid = header(2);
    invalid.codec = 0;
    assert!(matches!(
        FormatTracker::new(invalid, &[], None),
        Err(FormatError::Header)
    ));
}
#[test]
fn configuration_count_and_packet_bytes_are_bounded_and_failures_close_tracker() {
    let mut tracker = FormatTracker::new(header(2), &[], None).unwrap();
    for n in 0..8 {
        config(&mut tracker, &[n]);
    }
    for _ in 0..20 {
        config(&mut tracker, &[7]);
    } // duplicates do not consume slots
    assert!(matches!(
        tracker.plan(packet(CONFIG, 0, &[8])),
        Err(FormatError::Budget)
    ));
    assert!(matches!(tracker.recovery(), Err(FormatError::Closed)));
    assert!(matches!(tracker.current(), Err(FormatError::Closed)));
    assert!(matches!(
        tracker.plan(packet(KEY_FRAME, 0, &vp9_key(64, 64))),
        Err(FormatError::Closed)
    ));
    let too_large = vec![0; MAX_PAYLOAD + 1];
    assert!(FormatTracker::new(header(2), &[&too_large[..MAX_PAYLOAD]], None).is_ok());
    assert!(matches!(
        FormatTracker::new(header(2), &[], Some(&too_large)),
        Err(FormatError::Budget)
    ));
    assert!(matches!(
        FormatTracker::new(header(2), &[&[0][..]; 9], None),
        Err(FormatError::Budget)
    ));
    assert!(matches!(
        FormatTracker::new(header(2), &[&too_large], None),
        Err(FormatError::Budget)
    ));
    let mut tracker = FormatTracker::new(header(2), &[], None).unwrap();
    assert!(matches!(
        tracker.plan(packet(CONFIG, 0, &too_large)),
        Err(FormatError::Budget)
    ));
}
#[test]
fn known_oversize_or_wrong_packet_kind_closes_without_exporting_a_new_format() {
    let mut tracker = FormatTracker::new(header(2), &[], Some(&vp9_key(64, 64))).unwrap();
    let original = tracker.recovery().unwrap();
    assert!(matches!(
        tracker.plan(packet(KEY_FRAME, 0, &vp9_key(65536, 64))),
        Err(FormatError::Size)
    ));
    assert_eq!(size(&original), (64, 64));
    assert!(matches!(tracker.current(), Err(FormatError::Closed)));
    for flag in [
        crate::media::AUDIO,
        crate::media::ROTATION,
        crate::media::INTEGRITY,
    ] {
        let mut tracker = FormatTracker::new(header(2), &[], None).unwrap();
        assert!(matches!(
            tracker.plan(packet(flag, 0, &[0])),
            Err(FormatError::Kind)
        ));
        assert!(matches!(tracker.recovery(), Err(FormatError::Closed)));
    }
}
