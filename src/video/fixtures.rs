//! Shared synthetic syntax fixtures, adapted from Android SizeChangeFixtures.
//! Not full codec frames or a native decoder conformance claim.
#[derive(Default)]
pub(super) struct Writer(pub(super) Vec<u8>, pub(super) usize);
impl Writer {
    pub(super) fn put(&mut self, value: u32, count: usize) -> &mut Self {
        for bit in (0..count).rev() {
            if self.1 % 8 == 0 {
                self.0.push(0);
            }
            let last = self.0.last_mut().unwrap();
            *last |= (((value >> bit) & 1) as u8) << (7 - self.1 % 8);
            self.1 += 1;
        }
        self
    }
}
// Same profile-0 header/bit order as Android SizeChangeFixtures.vp9KeyFrame;
// the unused compressed filler is omitted. These helpers are syntax tests,
// not complete decodable pictures or an independently executed Android suite.
pub(super) fn vp9_key(width: u32, height: u32) -> Vec<u8> {
    let mut w = Writer::default();
    w.put(0x82, 8)
        .put(0x498342, 24)
        .put(2, 3)
        .put(0, 1)
        .put(width - 1, 16)
        .put(height - 1, 16)
        .put(0, 1);
    w.0
}
pub(super) fn obu_bytes(kind: u8, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() < 128);
    [&[kind << 3 | 2, payload.len() as u8], payload].concat()
}
// Bit-for-bit prefix from Android SizeChangeFixtures.av1SequenceHeaderObu.
pub(super) fn android_av1_sequence(width: u32, height: u32) -> Vec<u8> {
    let mut w = Writer::default();
    w.put(0, 3)
        .put(0, 1)
        .put(0, 1)
        .put(0, 1)
        .put(0, 1)
        .put(0, 5)
        .put(0, 12)
        .put(8, 5)
        .put(0, 1)
        .put(10, 4)
        .put(9, 4)
        .put(width - 1, 11)
        .put(height - 1, 10)
        .put(0, 1)
        .put(0, 1)
        .put(1, 1)
        .put(1, 1)
        .put(0, 4)
        .put(1, 1)
        .put(0, 1)
        .put(1, 1)
        .put(1, 1)
        .put(1, 1)
        .put(6, 3);
    obu_bytes(1, &w.0)
}
pub(super) fn frame(kind: u32, override_size: bool) -> Vec<u8> {
    let mut w = Writer::default();
    w.put(0, 1).put(kind, 2).put(1, 1);
    if kind != 0 && kind != 3 {
        w.put(1, 1);
    } // error resilience
    w.put(0, 1).put(0, 1); // cdf, screen tools
    if kind != 3 {
        w.put(override_size as u32, 1);
    }
    obu_bytes(6, &w.0)
}
