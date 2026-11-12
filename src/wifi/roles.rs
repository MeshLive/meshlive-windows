//! Capability bits, local `CAPS` and the deterministic role choice.

use super::wire::{Caps, bearer, caps_bit};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Host,
    Guest,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Roles {
    pub bearer: u8,
    pub local: Role,
}

/// What this PC says in its single CAPS: Direct group owner (bit 2) only, free
/// counts 0 (they count Aware data paths). `direct_possible_now` = switch on,
/// env arm on, probe fine, no hotspot, no Wi-Fi Direct group of this app, fault
/// pause not active (the caller decides).
pub fn windows_caps(direct_possible_now: bool) -> Caps {
    if direct_possible_now {
        Caps {
            bearers: caps_bit::DIRECT_GO,
            free_host: 0,
            free_guest: 0,
        }
    } else {
        Caps::default()
    }
}

/// W4G: the guest role's single CAPS: Wi-Fi Direct client (bit 3) only, free counts 0.
/// The phone hosts and this PC joins. `direct_possible_now` = env arm, guest role, probe
/// answered with Wi-Fi on and no Mobile Hotspot, no local-fault pause (the caller decides).
pub fn windows_guest_caps(direct_possible_now: bool) -> Caps {
    if direct_possible_now {
        Caps {
            bearers: caps_bit::DIRECT_CLIENT,
            free_host: 0,
            free_guest: 0,
        }
    } else {
        Caps::default()
    }
}

/// Deterministic from the two CAPS; both ends get the same bearer and opposite
/// `local`. Aware first, then Direct by bits 2/3 (free counts unused for
/// Direct), else None. A PC has bit 2 only, so it is always the host.
pub fn choose(local: &Caps, peer: &Caps, local_is_initiator: bool) -> Option<Roles> {
    let (initiator, responder) = if local_is_initiator {
        (local, peer)
    } else {
        (peer, local)
    };
    if initiator.bearers & caps_bit::AWARE != 0 && responder.bearers & caps_bit::AWARE != 0 {
        let host_is_responder = if responder.free_host > 0 && initiator.free_guest > 0 {
            Some(true)
        } else if initiator.free_host > 0 && responder.free_guest > 0 {
            Some(false)
        } else {
            None
        };
        if let Some(host_is_responder) = host_is_responder {
            return Some(Roles {
                bearer: bearer::AWARE,
                local: if host_is_responder != local_is_initiator {
                    Role::Host
                } else {
                    Role::Guest
                },
            });
        }
    }
    let host_is_responder = if responder.bearers & caps_bit::DIRECT_GO != 0
        && initiator.bearers & caps_bit::DIRECT_CLIENT != 0
    {
        Some(true)
    } else if initiator.bearers & caps_bit::DIRECT_GO != 0
        && responder.bearers & caps_bit::DIRECT_CLIENT != 0
    {
        Some(false)
    } else {
        None
    }?;
    Some(Roles {
        bearer: bearer::DIRECT,
        local: if host_is_responder != local_is_initiator {
            Role::Host
        } else {
            Role::Guest
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(bearers: u8, free_host: u8, free_guest: u8) -> Caps {
        Caps {
            bearers,
            free_host,
            free_guest,
        }
    }

    #[test]
    fn windows_is_the_host_for_a_phone_that_can_join_whoever_started_the_call() {
        let phone = caps(caps_bit::DIRECT_CLIENT, 0, 0);
        let windows = windows_caps(true);
        let expected = Some(Roles {
            bearer: bearer::DIRECT,
            local: Role::Host,
        });
        assert_eq!(choose(&windows, &phone, true), expected);
        assert_eq!(choose(&windows, &phone, false), expected);
    }

    #[test]
    fn windows_with_everything_off_offers_nothing_but_still_has_a_caps() {
        assert_eq!(windows_caps(false), Caps::default());
        let peer = caps(caps_bit::AWARE | caps_bit::DIRECT_GO | caps_bit::DIRECT_CLIENT, 1, 1);
        assert_eq!(choose(&windows_caps(false), &peer, true), None);
        assert_eq!(choose(&peer, &windows_caps(false), false), None);
    }

    #[test]
    fn windows_never_picks_aware_even_when_the_phone_offers_it() {
        let phone = caps(caps_bit::AWARE | caps_bit::DIRECT_CLIENT, 1, 1);
        let windows = windows_caps(true);
        for local_is_initiator in [true, false] {
            assert_eq!(
                choose(&windows, &phone, local_is_initiator),
                Some(Roles {
                    bearer: bearer::DIRECT,
                    local: Role::Host,
                })
            );
        }
    }

    #[test]
    fn a_failed_aware_tuple_falls_through_to_direct() {
        let a = caps(
            caps_bit::AWARE | caps_bit::DIRECT_GO | caps_bit::DIRECT_CLIENT,
            0,
            0,
        );
        let b = a;
        for local_is_initiator in [true, false] {
            assert_eq!(
                choose(&a, &b, local_is_initiator).map(|roles| roles.bearer),
                Some(bearer::DIRECT)
            );
        }
    }

    #[test]
    fn two_phones_with_aware_use_the_revision_2_tuple() {
        let responder = caps(caps_bit::AWARE, 1, 0);
        let initiator = caps(caps_bit::AWARE, 0, 1);
        assert_eq!(
            choose(&initiator, &responder, true),
            Some(Roles {
                bearer: bearer::AWARE,
                local: Role::Guest,
            })
        );
        assert_eq!(
            choose(&responder, &initiator, false),
            Some(Roles {
                bearer: bearer::AWARE,
                local: Role::Host,
            })
        );

        let responder = caps(caps_bit::AWARE, 0, 1);
        let initiator = caps(caps_bit::AWARE, 1, 0);
        assert_eq!(
            choose(&initiator, &responder, true),
            Some(Roles {
                bearer: bearer::AWARE,
                local: Role::Host,
            })
        );
        assert_eq!(
            choose(&responder, &initiator, false),
            Some(Roles {
                bearer: bearer::AWARE,
                local: Role::Guest,
            })
        );
    }

    #[test]
    fn a_phone_that_is_also_a_host_only_does_not_pair_with_windows() {
        let phone = caps(caps_bit::DIRECT_GO, 0, 0);
        let windows = windows_caps(true);
        assert_eq!(choose(&windows, &phone, true), None);
        assert_eq!(choose(&windows, &phone, false), None);
    }

    #[test]
    fn a_guest_pc_joins_a_phone_that_can_host_whoever_started_the_call() {
        let phone = caps(caps_bit::DIRECT_GO | caps_bit::DIRECT_CLIENT, 0, 0);
        let pc = windows_guest_caps(true);
        let expected = Some(Roles {
            bearer: bearer::DIRECT,
            local: Role::Guest,
        });
        assert_eq!(choose(&pc, &phone, true), expected);
        assert_eq!(choose(&pc, &phone, false), expected);
        assert_eq!(choose(&phone, &pc, true).map(|roles| roles.local), Some(Role::Host));
        assert_eq!(choose(&phone, &pc, false).map(|roles| roles.local), Some(Role::Host));
    }

    #[test]
    fn a_guest_pc_never_offers_to_host_and_offers_nothing_when_it_cannot_join() {
        assert_eq!(windows_guest_caps(false), Caps::default());
        let pc = windows_guest_caps(true);
        assert_eq!(pc.bearers & caps_bit::DIRECT_GO, 0);
        // Two guests have no host: no tuple, both stay on Bluetooth.
        assert_eq!(choose(&pc, &windows_guest_caps(true), true), None);
        assert_eq!(choose(&pc, &caps(caps_bit::DIRECT_CLIENT, 0, 0), false), None);
        // A phone that also offers Wi-Fi Aware still gets the Direct tuple with the PC as guest.
        let phone = caps(caps_bit::AWARE | caps_bit::DIRECT_GO | caps_bit::DIRECT_CLIENT, 1, 1);
        assert_eq!(
            choose(&pc, &phone, false),
            Some(Roles {
                bearer: bearer::DIRECT,
                local: Role::Guest,
            })
        );
    }

    #[test]
    fn both_sides_agree() {
        for bearers in [0, 1, 2, 3, 4, 5, 8, 9, 12, 13] {
            for free_host in 0..=2 {
                for free_guest in 0..=2 {
                    let a = caps(bearers, free_host, free_guest);
                    let b = caps(
                        bearers ^ 0x0c,
                        free_guest,
                        free_host,
                    );
                    let left = choose(&a, &b, true);
                    let right = choose(&b, &a, false);
                    match (left, right) {
                        (None, None) => {}
                        (Some(left), Some(right)) => {
                            assert_eq!(left.bearer, right.bearer);
                            assert_ne!(left.local, right.local);
                        }
                        (left, right) => {
                            panic!("one side is None: {left:?} {right:?}")
                        }
                    }
                }
            }
        }
    }
}
