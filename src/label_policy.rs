//! When this PC's name is sent in a call, and to whom (Android
//! `CallLabelPolicy`, Design G / G1; PROTOCOL "Naming v2 / Label timing and
//! audience"). No new bytes: `BNL1`, the `BLB1` container, hello bit 5 and the
//! receiver checks are unchanged; this is only the moment a sender queues its
//! name, decided in one place. Pure.
//!
//! - Audience ("Who sees my name"): everyone (default), only verified
//!   contacts, nobody. No call subject is ever sent (kind 3 retired).
//! - Caller, stranger session (the callee presented a throwaway key): with
//!   "Show my name before they answer" (always on since 2026-11-04) the name
//!   goes before the Invite, signed by the attempt pseudonym; without it, after
//!   the callee's verified REVEAL.
//! - Callee: "everyone" sends at Ringing to a verified contact, else with
//!   Accept. "Only my contacts" cannot tell a stranger from a contact whose
//!   lease expired, so a pseudonymous callee decides only after the caller's
//!   verified REVEAL. Declining or letting it ring reveals nothing.
//! - A peer without bit 5 gets nothing; nothing breaks.

/// Who sees this PC's name (Settings, shown only while a name is set).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NamesAudience {
    /// "People I call or who call me".
    #[default]
    Everyone,
    /// "Only my contacts".
    VerifiedOnly,
    /// "Nobody".
    Nobody,
}
impl NamesAudience {
    pub const ALL: [NamesAudience; 3] = [NamesAudience::Everyone, NamesAudience::VerifiedOnly, NamesAudience::Nobody];
    /// The saved value (Android's preference values).
    pub fn stored(self) -> &'static str {
        match self {
            NamesAudience::Everyone => "everyone",
            NamesAudience::VerifiedOnly => "verified",
            NamesAudience::Nobody => "nobody",
        }
    }
    /// A saved value; anything else (missing, damaged, from a newer build) is everyone.
    pub fn from_stored(value: Option<&str>) -> Self {
        NamesAudience::ALL.into_iter().find(|a| Some(a.stored()) == value).unwrap_or_default()
    }
}

/// When one label is queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendMoment {
    /// Never in this call.
    Never,
    /// At once: before the Invite (caller) or as soon as the session allows.
    Now,
    /// After a relayed callee proved it owns the offer by its OPENING.
    AfterOpening,
    /// After the callee's REVEAL is verified (the Design F rule).
    AfterReveal,
    /// After the peer's REVEAL is verified and it is a verified contact; otherwise never.
    AfterRevealIfVerified,
    /// Callee: with Ringing (the caller is already a verified contact).
    AtRinging,
    /// Callee: with Accept.
    AtAccept,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallerContext {
    /// Both endpoint hellos carry bit 5.
    pub labels_negotiated: bool,
    pub has_name: bool,
    pub audience: NamesAudience,
    /// "Show my name before they answer" (always true on this PC since 2026-11-04).
    pub name_before_answer: bool,
    /// The callee presented a throwaway key and its reveal is still awaited.
    pub callee_awaits_reveal: bool,
    /// A relayed callee proven only by its OPENING, which follows the Invite.
    pub relayed_awaits_opening: bool,
    /// The callee's real key is known now and is a verified contact's.
    pub callee_is_verified_contact: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CalleeContext {
    pub labels_negotiated: bool,
    pub has_name: bool,
    pub audience: NamesAudience,
    /// This PC presented a throwaway key: the caller is unknown until its reveal.
    pub own_pseudonymous: bool,
    /// The caller's real key is known now and is a verified contact's.
    pub caller_is_verified_contact: bool,
}

/// The caller's moment for its name (Android `callerPlan`).
pub fn caller_plan(c: &CallerContext) -> SendMoment {
    if !c.labels_negotiated || !c.has_name || c.audience == NamesAudience::Nobody {
        return SendMoment::Never;
    }
    if c.audience == NamesAudience::VerifiedOnly {
        return if c.callee_awaits_reveal {
            SendMoment::AfterRevealIfVerified
        } else if !c.callee_is_verified_contact {
            SendMoment::Never
        } else if c.relayed_awaits_opening {
            SendMoment::AfterOpening
        } else {
            SendMoment::Now
        };
    }
    if c.callee_awaits_reveal {
        if c.name_before_answer { SendMoment::Now } else { SendMoment::AfterReveal }
    } else if c.relayed_awaits_opening {
        SendMoment::AfterOpening
    } else {
        SendMoment::Now
    }
}

/// The callee's moment for its name (Android `calleeMoment`).
pub fn callee_moment(c: &CalleeContext) -> SendMoment {
    if !c.labels_negotiated || !c.has_name {
        return SendMoment::Never;
    }
    match c.audience {
        NamesAudience::Nobody => SendMoment::Never,
        NamesAudience::Everyone => {
            if c.caller_is_verified_contact { SendMoment::AtRinging } else { SendMoment::AtAccept }
        }
        NamesAudience::VerifiedOnly => {
            if c.caller_is_verified_contact {
                SendMoment::AtRinging
            } else if c.own_pseudonymous {
                SendMoment::AfterRevealIfVerified
            } else {
                SendMoment::Never
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn caller(audience: NamesAudience, negotiated: bool, name: bool, before_answer: bool, awaits_reveal: bool, relayed: bool, verified_contact: bool) -> SendMoment {
        caller_plan(&CallerContext {
            labels_negotiated: negotiated,
            has_name: name,
            audience,
            name_before_answer: before_answer,
            callee_awaits_reveal: awaits_reveal,
            relayed_awaits_opening: relayed,
            callee_is_verified_contact: verified_contact,
        })
    }
    fn callee(audience: NamesAudience, negotiated: bool, name: bool, own_pseudonymous: bool, caller_verified: bool) -> SendMoment {
        callee_moment(&CalleeContext { labels_negotiated: negotiated, has_name: name, audience, own_pseudonymous, caller_is_verified_contact: caller_verified })
    }
    use NamesAudience::{Everyone, Nobody, VerifiedOnly};
    use SendMoment::*;

    /// A peer without bit 5 gets nothing from either side; nothing breaks.
    #[test]
    fn a_peer_without_bit_5_gets_nothing_from_either_side() {
        // A 0.2.0-beta phone with names off (its default) never advertises bit 5: nothing is sent, nothing breaks.
        assert_eq!(caller(Everyone, false, true, true, false, false, false), Never);
        assert_eq!(callee(Everyone, false, true, false, false), Never);
    }

    #[test]
    fn a_stranger_session_with_show_my_name_before_they_answer_sends_the_name_now() {
        assert_eq!(caller(Everyone, true, true, true, true, false, false), Now);
    }

    #[test]
    fn the_held_arm_keeps_the_design_f_rule() {
        assert_eq!(caller(Everyone, true, true, false, true, false, false), AfterReveal);
    }

    #[test]
    fn a_real_key_session_sends_the_name_now() {
        assert_eq!(caller(Everyone, true, true, true, false, false, false), Now);
    }

    #[test]
    fn a_relayed_callee_waits_for_its_opening_as_before() {
        assert_eq!(caller(Everyone, true, true, true, false, true, false), AfterOpening);
    }

    #[test]
    fn nothing_is_sent_when_there_is_no_name() {
        assert_eq!(caller(Everyone, true, false, true, false, false, false), Never);
    }

    #[test]
    fn audience_nobody_sends_no_name() {
        assert_eq!(caller(Nobody, true, true, true, false, false, false), Never);
        assert_eq!(caller(Nobody, true, true, true, true, false, false), Never);
    }

    #[test]
    fn audience_verified_only_gives_the_name_to_a_verified_contact_and_to_no_stranger() {
        assert_eq!(caller(VerifiedOnly, true, true, true, false, false, true), Now);
        assert_eq!(caller(VerifiedOnly, true, true, true, false, false, false), Never);
        // A pseudonymous callee might be a contact whose lease expired: decided only after its verified reveal.
        assert_eq!(caller(VerifiedOnly, true, true, true, true, false, false), AfterRevealIfVerified);
        assert_eq!(caller(VerifiedOnly, true, true, true, false, true, true), AfterOpening);
    }

    #[test]
    fn the_name_before_answer_setting_changes_nothing_for_a_verified_or_real_key_call() {
        for before in [true, false] {
            assert_eq!(caller(Everyone, true, true, before, false, false, false), Now);
            assert_eq!(caller(VerifiedOnly, true, true, before, false, false, true), Now);
        }
    }

    #[test]
    fn the_callee_keeps_the_existing_rule_for_everyone() {
        assert_eq!(callee(Everyone, true, true, false, true), AtRinging);
        assert_eq!(callee(Everyone, true, true, false, false), AtAccept);
        assert_eq!(callee(Everyone, true, true, true, false), AtAccept);
    }

    #[test]
    fn with_verified_only_a_stranger_caller_gets_no_name_and_a_pseudonym_session_waits_for_the_callers_reveal() {
        // A real key, not verified: a stranger. Nothing is ever sent.
        assert_eq!(callee(VerifiedOnly, true, true, false, false), Never);
        // A real-key verified contact: at Ringing.
        assert_eq!(callee(VerifiedOnly, true, true, false, true), AtRinging);
        // The callee presented a throwaway (lease-expired contacts look like strangers): decided at the caller's reveal.
        assert_eq!(callee(VerifiedOnly, true, true, true, false), AfterRevealIfVerified);
    }

    #[test]
    fn the_callee_sends_nothing_for_nobody_or_without_a_name() {
        assert_eq!(callee(Nobody, true, true, false, true), Never);
        assert_eq!(callee(Everyone, true, false, false, true), Never);
    }

    #[test]
    fn declining_or_letting_it_ring_reveals_nothing_because_no_moment_is_before_accept_for_a_stranger() {
        // The only moment before Accept is AT_RINGING, which is reachable only with callerIsVerifiedContact.
        for audience in NamesAudience::ALL {
            for pseudo in [true, false] {
                assert!(callee(audience, true, true, pseudo, false) != AtRinging);
            }
        }
    }

    #[test]
    fn audience_defaults_to_everyone_and_parses_the_stored_value() {
        assert_eq!(NamesAudience::from_stored(None), Everyone);
        assert_eq!(NamesAudience::from_stored(Some("garbage")), Everyone);
        assert_eq!(NamesAudience::from_stored(Some("verified")), VerifiedOnly);
        assert_eq!(NamesAudience::from_stored(Some("nobody")), Nobody);
        for a in NamesAudience::ALL {
            assert_eq!(NamesAudience::from_stored(Some(a.stored())), a);
        }
    }
}
