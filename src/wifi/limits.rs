//! Wi-Fi limits shared with the Android implementation (A0 `WifiLimits`).
//! All time values are milliseconds in `i64` unless the name ends `_NS`/`_BYTES`/`_KBPS`.

pub const HOST_VALID_MS: u16 = 20_000; // HOST validity (also the u16 on the wire)
pub const START_DEADLINE_MS: i64 = 10_000; // group Started + adapter IPv4 before HOST
pub const DISCOVERY_TO_PICK_MS: i64 = 8_000; // A0 name parity; unused on Windows
pub const ATTEMPT_MAX_MS: i64 = 25_000; // HOST to the first record on the new carrier
pub const BIND_FRAME2_DEADLINE_MS: i64 = 2_000; // frame 2 within 2 s of accept
pub const BIND_READY_DEADLINE_MS: i64 = 5_000; // BIND and READY both ways within 5 s of accept
pub const MAX_ACCEPTED_SOCKETS_PER_ATTEMPT: u32 = 4;
pub const RETRY_DELAY_MS: i64 = 30_000;
pub const MAX_FAILED_ATTEMPTS_PER_HOP: u32 = 2;
pub const LOCAL_FAULT_LIMIT: usize = 3; // local platform failures ...
pub const LOCAL_FAULT_WINDOW_MS: i64 = 3_600_000; // ... within 60 min
pub const LOCAL_FAULT_PAUSE_MS: i64 = 3_600_000; // pause all attempts for 60 min
pub const HEARTBEAT_IDLE_MS: i64 = 500; // Wi-Fi carrier idle heartbeat
pub const CARRIER_DEAD_SILENCE_MS: i64 = 2_500; // no complete authenticated record
pub const CARRIER_DEAD_STALL_MS: i64 = 2_500; // write made no progress
pub const CUTOVER_HOLD_MS: i64 = 1_000; // cutover hold
pub const CUTOVER_HOLD_BYTES: i64 = 5 * 1024 * 1024;
pub const CUTOVER_OLD_QUEUE_GATE_MS: i64 = 500; // = CUTOVER_HOLD_MS / 2, informational
pub const COOLDOWN_AFTER_FALLBACK_MS: i64 = 30_000;
pub const MAX_FALLBACKS_PER_HOP: i32 = 2;
pub const ACTIVE_BEFORE_SWITCH_MS: i64 = 3_000; // call active with video this long before an attempt
pub const PENDING_MAX_MS: i64 = 5_000; // machine pending_max_ms = BIND_READY_DEADLINE_MS
pub const STANDBY_FRESH_MS: i64 = 3_000; // A3's soft-stall threshold: a standby is fresh when heard within this
pub const SOCKET_SEND_BUFFER_BYTES: usize = 256 * 1024; // TCP send buffer
pub const OUT_BUFFER_MAX_BYTES: usize = 8 * 1024 * 1024; // above this the carrier counts as stalled
pub const MAX_BWF1_PLAINTEXT_BYTES: usize = 96;
pub const MIN_BWF1_PLAINTEXT_BYTES: usize = 5; // magic 4 + kind 1
pub const MIN_5GHZ_MHZ: u16 = 4_900; // a Direct HOST frequency is 0 or at least this
pub const MAX_FREE_PATHS_IN_CAPS: u8 = 7;
// No BWF1_MAX_CIPHERTEXT: `wire::accepts_bwf1_ciphertext_length` enforces 21..=112.
pub const READY_RECV_CEILING_KBPS: u16 = 1100;
pub const READY_FAMILY_CAP_KBPS: u16 = 0;
pub const READY_TRANSPORT_BUDGET_KBPS: u16 = 1100;
pub const WINDOWS_DIRECT_FREQ_MHZ: u16 = 0; // Windows cannot read or set it (the phone reads the group frequency)
// W4G guest role (Android's guest: WifiHopAgent / DirectBearerDriver).
pub const GUEST_CREDS_DEADLINE_MS: i64 = 8_000; // CREDS after HOST
pub const GUEST_JOIN_DEADLINE_MS: i64 = 12_000; // joined and an address on the joined interface, from the join
pub const GUEST_CONNECT_WINDOW_MS: i64 = 5_000; // connect (with retries) from the first connect
pub const GUEST_CONNECT_RETRY_MS: i64 = 250; // a refused connect is retried this much later
pub const GUEST_FRAME_DEADLINE_MS: i64 = 2_000; // BWU1 frame 1 after connect, frame 3 after frame 2
pub const GUEST_MAX_HOSTS_PER_HOP: u32 = 6; // HOSTs accepted per call; later ones are declined 4
pub const GUEST_REST_MS: i64 = 29_000; // a HOST this soon after the last attempt ended is declined 4
