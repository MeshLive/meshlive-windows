//! A small QR Code encoder (ISO/IEC 18004) for this PC's BVMID2 code: the
//! alphanumeric mode (uppercase hex and '.'), error correction level M,
//! versions 1-10, the mask with the lowest penalty. The construction follows
//! the public-domain reference by Project Nayuki ("QR Code generator
//! library"); checked here against the worked "HELLO WORLD" 1-M example
//! (Thonky QR Code tutorial) and the standard format and version bit tables.
//! No other mode or level is needed: the code text is ~150 characters (v6-M).

const ALPHANUMERIC: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ $%*+-./:";
/// Level M, versions 1..=10 (index 0 unused).
const ECC_PER_BLOCK: [usize; 11] = [0, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26];
const BLOCKS: [usize; 11] = [0, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5];
const MAX_VERSION: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrCode {
    pub size: usize,
    pub version: usize,
    pub mask: u8,
    /// Row-major, true = dark.
    modules: Vec<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A character outside the QR alphanumeric set.
    Character,
    /// Longer than version 10 at level M holds.
    TooLong,
}

impl QrCode {
    pub fn get(&self, x: usize, y: usize) -> bool {
        self.modules[y * self.size + x]
    }
    /// Encodes `text` (QR alphanumeric characters only).
    pub fn encode(text: &str) -> Result<Self, Error> {
        let values: Vec<u32> = text.chars().map(|c| ALPHANUMERIC.find(c).map(|i| i as u32).ok_or(Error::Character)).collect::<Result<_, _>>()?;
        let version = (1..=MAX_VERSION)
            .find(|&v| segment_bits(values.len(), v) <= data_codewords(v) * 8)
            .ok_or(Error::TooLong)?;
        let data = data_bytes(&values, version);
        let codewords = add_ecc_and_interleave(&data, version);
        let mut qr = Builder::new(version);
        qr.function_patterns();
        qr.codewords(&codewords);
        // The lowest-penalty mask (any mask decodes; this one scans best).
        let mut best: Option<(u32, u8)> = None;
        for mask in 0..8u8 {
            qr.apply_mask(mask);
            qr.format_bits(mask);
            let penalty = qr.penalty();
            if best.is_none_or(|(p, _)| penalty < p) {
                best = Some((penalty, mask));
            }
            qr.apply_mask(mask); // XOR undoes it
        }
        let mask = best.unwrap().1;
        qr.apply_mask(mask);
        qr.format_bits(mask);
        Ok(Self { size: qr.size, version, mask, modules: qr.modules })
    }
}

fn count_bits(version: usize) -> usize {
    if version <= 9 { 9 } else { 11 }
}
fn segment_bits(chars: usize, version: usize) -> usize {
    4 + count_bits(version) + chars / 2 * 11 + chars % 2 * 6
}
fn raw_modules(version: usize) -> usize {
    let mut result = (16 * version + 128) * version + 64;
    if version >= 2 {
        let align = version / 7 + 2;
        result -= (25 * align - 10) * align - 55;
        if version >= 7 {
            result -= 36;
        }
    }
    result
}
fn data_codewords(version: usize) -> usize {
    raw_modules(version) / 8 - ECC_PER_BLOCK[version] * BLOCKS[version]
}

struct Bits(Vec<bool>);
impl Bits {
    fn push(&mut self, value: u32, count: usize) {
        for i in (0..count).rev() {
            self.0.push((value >> i) & 1 != 0);
        }
    }
}

/// Mode, count, data, terminator and padding: exactly the data codewords.
fn data_bytes(values: &[u32], version: usize) -> Vec<u8> {
    let capacity = data_codewords(version) * 8;
    let mut bits = Bits(Vec::new());
    bits.push(0b0010, 4);
    bits.push(values.len() as u32, count_bits(version));
    for pair in values.chunks(2) {
        match pair {
            [a, b] => bits.push(a * 45 + b, 11),
            [a] => bits.push(*a, 6),
            _ => {}
        }
    }
    let terminator = (capacity - bits.0.len()).min(4);
    bits.push(0, terminator);
    let pad = (8 - bits.0.len() % 8) % 8;
    bits.push(0, pad);
    let mut pad_byte = 0xEC;
    while bits.0.len() < capacity {
        bits.push(pad_byte, 8);
        pad_byte ^= 0xEC ^ 0x11;
    }
    bits.0.chunks(8).map(|c| c.iter().fold(0u8, |acc, &b| (acc << 1) | u8::from(b))).collect()
}

fn gf_mul(x: u8, y: u8) -> u8 {
    let mut z: u32 = 0;
    for i in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11D);
        z ^= ((u32::from(y) >> i) & 1) * u32::from(x);
    }
    z as u8
}
fn rs_divisor(degree: usize) -> Vec<u8> {
    let mut result = vec![0u8; degree];
    result[degree - 1] = 1;
    let mut root = 1u8;
    for _ in 0..degree {
        for j in 0..degree {
            result[j] = gf_mul(result[j], root);
            if j + 1 < degree {
                result[j] ^= result[j + 1];
            }
        }
        root = gf_mul(root, 0x02);
    }
    result
}
fn rs_remainder(data: &[u8], divisor: &[u8]) -> Vec<u8> {
    let mut result = vec![0u8; divisor.len()];
    for &b in data {
        let factor = b ^ result.remove(0);
        result.push(0);
        for (r, d) in result.iter_mut().zip(divisor) {
            *r ^= gf_mul(*d, factor);
        }
    }
    result
}

fn add_ecc_and_interleave(data: &[u8], version: usize) -> Vec<u8> {
    let blocks = BLOCKS[version];
    let ecc_len = ECC_PER_BLOCK[version];
    let raw = raw_modules(version) / 8;
    let short_blocks = blocks - raw % blocks;
    let short_len = raw / blocks;
    let divisor = rs_divisor(ecc_len);
    let mut all = Vec::new();
    let mut k = 0;
    for i in 0..blocks {
        let len = short_len - ecc_len + usize::from(i >= short_blocks);
        let mut block = data[k..k + len].to_vec();
        k += len;
        let ecc = rs_remainder(&block, &divisor);
        if i < short_blocks {
            block.push(0);
        }
        block.extend(ecc);
        all.push(block);
    }
    let mut result = Vec::new();
    for i in 0..all[0].len() {
        for (j, block) in all.iter().enumerate() {
            if i != short_len - ecc_len || j >= short_blocks {
                result.push(block[i]);
            }
        }
    }
    result
}

/// Format bits for level M and `mask` (15 bits, BCH and the 0x5412 XOR).
pub fn format_bits(mask: u8) -> u32 {
    let data = u32::from(mask); // level M = 00
    let mut rem = data;
    for _ in 0..10 {
        rem = (rem << 1) ^ ((rem >> 9) * 0x537);
    }
    ((data << 10) | rem) ^ 0x5412
}
/// Version information bits (versions 7+; 18 bits).
pub fn version_bits(version: usize) -> u32 {
    let mut rem = version as u32;
    for _ in 0..12 {
        rem = (rem << 1) ^ ((rem >> 11) * 0x1F25);
    }
    ((version as u32) << 12) | rem
}

fn alignment_positions(version: usize) -> Vec<usize> {
    if version == 1 {
        return Vec::new();
    }
    let count = version / 7 + 2;
    let size = version * 4 + 17;
    let step = (version * 4 + count * 2 + 1) / (count * 2 - 2) * 2;
    let mut result = vec![0; count];
    result[0] = 6;
    let mut pos = size - 7;
    for i in (1..count).rev() {
        result[i] = pos;
        pos -= step;
    }
    result
}

struct Builder {
    version: usize,
    size: usize,
    modules: Vec<bool>,
    function: Vec<bool>,
}
impl Builder {
    fn new(version: usize) -> Self {
        let size = version * 4 + 17;
        Self { version, size, modules: vec![false; size * size], function: vec![false; size * size] }
    }
    fn set(&mut self, x: usize, y: usize, dark: bool) {
        self.modules[y * self.size + x] = dark;
        self.function[y * self.size + x] = true;
    }
    fn function_patterns(&mut self) {
        let size = self.size;
        for i in 0..size {
            self.set(6, i, i % 2 == 0);
            self.set(i, 6, i % 2 == 0);
        }
        for (x, y) in [(3, 3), (size - 4, 3), (3, size - 4)] {
            for dy in -4i32..=4 {
                for dx in -4i32..=4 {
                    let (xx, yy) = (x as i32 + dx, y as i32 + dy);
                    if (0..size as i32).contains(&xx) && (0..size as i32).contains(&yy) {
                        let dist = dx.abs().max(dy.abs());
                        self.set(xx as usize, yy as usize, dist != 2 && dist != 4);
                    }
                }
            }
        }
        let positions = alignment_positions(self.version);
        let last = positions.len().saturating_sub(1);
        for (i, &px) in positions.iter().enumerate() {
            for (j, &py) in positions.iter().enumerate() {
                if (i == 0 && j == 0) || (i == 0 && j == last) || (i == last && j == 0) {
                    continue;
                }
                for dy in -2i32..=2 {
                    for dx in -2i32..=2 {
                        self.set((px as i32 + dx) as usize, (py as i32 + dy) as usize, dx.abs().max(dy.abs()) != 1);
                    }
                }
            }
        }
        self.format_bits(0); // reserves the areas; drawn again after masking
        if self.version >= 7 {
            let bits = version_bits(self.version);
            for i in 0..18 {
                let dark = (bits >> i) & 1 != 0;
                let (a, b) = (size - 11 + i % 3, i / 3);
                self.set(a, b, dark);
                self.set(b, a, dark);
            }
        }
    }
    fn format_bits(&mut self, mask: u8) {
        let bits = format_bits(mask);
        let bit = |i: usize| (bits >> i) & 1 != 0;
        let size = self.size;
        for i in 0..=5 {
            self.set(8, i, bit(i));
        }
        self.set(8, 7, bit(6));
        self.set(8, 8, bit(7));
        self.set(7, 8, bit(8));
        for i in 9..15 {
            self.set(14 - i, 8, bit(i));
        }
        for i in 0..8 {
            self.set(size - 1 - i, 8, bit(i));
        }
        for i in 8..15 {
            self.set(8, size - 15 + i, bit(i));
        }
        self.set(8, size - 8, true);
    }
    fn codewords(&mut self, data: &[u8]) {
        let size = self.size;
        let mut i = 0usize;
        let mut right = size as i32 - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            for vert in 0..size {
                for j in 0..2 {
                    let x = (right - j) as usize;
                    let upward = (right + 1) & 2 == 0;
                    let y = if upward { size - 1 - vert } else { vert };
                    if !self.function[y * size + x] && i < data.len() * 8 {
                        self.modules[y * size + x] = (data[i >> 3] >> (7 - (i & 7))) & 1 != 0;
                        i += 1;
                    }
                }
            }
            right -= 2;
        }
    }
    fn apply_mask(&mut self, mask: u8) {
        for y in 0..self.size {
            for x in 0..self.size {
                let invert = match mask {
                    0 => (x + y) % 2 == 0,
                    1 => y % 2 == 0,
                    2 => x % 3 == 0,
                    3 => (x + y) % 3 == 0,
                    4 => (x / 3 + y / 2) % 2 == 0,
                    5 => x * y % 2 + x * y % 3 == 0,
                    6 => (x * y % 2 + x * y % 3) % 2 == 0,
                    _ => ((x + y) % 2 + x * y % 3) % 2 == 0,
                };
                let at = y * self.size + x;
                if invert && !self.function[at] {
                    self.modules[at] ^= true;
                }
            }
        }
    }
    /// Penalty rules 1 (runs of 5+), 2 (2x2 blocks), 3 (finder-like
    /// 1:1:3:1:1 runs with 4 light) and 4 (dark balance).
    fn penalty(&self) -> u32 {
        let n = self.size;
        let at = |x: usize, y: usize| self.modules[y * n + x];
        let mut score = 0u32;
        let line = |get: &dyn Fn(usize) -> bool| -> u32 {
            let mut s = 0;
            let mut run = 1;
            for i in 1..n {
                if get(i) == get(i - 1) {
                    run += 1;
                } else {
                    if run >= 5 {
                        s += 3 + (run - 5) as u32;
                    }
                    run = 1;
                }
            }
            if run >= 5 {
                s += 3 + (run - 5) as u32;
            }
            let pattern = [true, false, true, true, true, false, true];
            for i in 0..n.saturating_sub(6) {
                if (0..7).all(|k| get(i + k) == pattern[k]) {
                    let before = i >= 4 && (i - 4..i).all(|k| !get(k));
                    let after = i + 11 <= n && (i + 7..i + 11).all(|k| !get(k));
                    if before || after {
                        s += 40;
                    }
                }
            }
            s
        };
        for y in 0..n {
            score += line(&|x| at(x, y));
        }
        for x in 0..n {
            score += line(&|y| at(x, y));
        }
        for y in 0..n - 1 {
            for x in 0..n - 1 {
                let c = at(x, y);
                if c == at(x + 1, y) && c == at(x, y + 1) && c == at(x + 1, y + 1) {
                    score += 3;
                }
            }
        }
        let dark = self.modules.iter().filter(|&&d| d).count();
        let total = n * n;
        let k = ((dark * 20).abs_diff(total * 10)).div_ceil(total).saturating_sub(1);
        score + (k as u32) * 10
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes matrices for an external decoder check (run with --ignored and
    /// MESHLIVE_QR_DUMP=<file>): one line per code, "text|size|rows of 0/1".
    #[test]
    #[ignore]
    fn dump_for_an_external_decoder() {
        let Some(path) = std::env::var_os("MESHLIVE_QR_DUMP") else { return };
        let mut out = String::new();
        for seed in 0u8..12 {
            let hexes = |b: u8| (0..32).map(|i| format!("{:02X}", b.wrapping_mul(31).wrapping_add(i * 7))).collect::<String>();
            let text = format!("BVMID2.{}.{}.{}", hexes(seed), hexes(seed ^ 0x5a), &hexes(seed.wrapping_add(3))[..12]);
            let qr = QrCode::encode(&text).unwrap();
            let rows: Vec<String> = (0..qr.size).map(|y| (0..qr.size).map(|x| if qr.get(x, y) { '1' } else { '0' }).collect()).collect();
            out.push_str(&format!("{text}|{}|{}|{}
", qr.size, qr.mask, rows.join(",")));
        }
        for text in ["HELLO WORLD", "BVMID2"] {
            let qr = QrCode::encode(text).unwrap();
            let rows: Vec<String> = (0..qr.size).map(|y| (0..qr.size).map(|x| if qr.get(x, y) { '1' } else { '0' }).collect()).collect();
            out.push_str(&format!("{text}|{}|{}|{}
", qr.size, qr.mask, rows.join(",")));
        }
        std::fs::write(path, out).unwrap();
    }

    #[test]
    fn hello_world_1m_matches_the_worked_example() {
        // Thonky QR tutorial: "HELLO WORLD", version 1, level M.
        let values: Vec<u32> = "HELLO WORLD".chars().map(|c| ALPHANUMERIC.find(c).unwrap() as u32).collect();
        let data = data_bytes(&values, 1);
        assert_eq!(data, [32, 91, 11, 120, 209, 114, 220, 77, 67, 64, 236, 17, 236, 17, 236, 17]);
        let ecc = rs_remainder(&data, &rs_divisor(10));
        assert_eq!(ecc, [196, 35, 39, 119, 235, 215, 231, 226, 93, 23]);
        let qr = QrCode::encode("HELLO WORLD").unwrap();
        assert_eq!((qr.version, qr.size), (1, 21));
    }

    #[test]
    fn format_and_version_bits_match_the_standard_tables() {
        assert_eq!(format_bits(0), 0b101010000010010); // M, mask 0
        assert_eq!(format_bits(5), 0b100000011001110); // M, mask 5
        assert_eq!(format_bits(7), 0b100101010100000); // M, mask 7
        assert_eq!(version_bits(7), 0x07C94);
        assert_eq!(version_bits(10), 0x0A4D3);
        assert_eq!(alignment_positions(7), vec![6, 22, 38]);
        assert_eq!(alignment_positions(2), vec![6, 18]);
        assert_eq!(alignment_positions(10), vec![6, 28, 50]);
    }

    #[test]
    fn a_bvmid2_code_fits_version_6_m_with_finders_and_every_data_bit_placed() {
        let text = format!("BVMID2.{}.{}.{}", "AB".repeat(32), "CD".repeat(32), "EF0123456789");
        let qr = QrCode::encode(&text).unwrap();
        assert_eq!(qr.version, 6);
        assert_eq!(qr.size, 41);
        // Finder corners: dark outer ring, light second ring, dark core.
        for (x, y) in [(0, 0), (34, 0), (0, 34)] {
            assert!(qr.get(x, y) && qr.get(x + 6, y + 6) && qr.get(x + 3, y + 3));
            assert!(!qr.get(x + 1, y + 1));
        }
        // Dark module and timing.
        assert!(qr.get(8, qr.size - 8));
        assert!(qr.get(8, 6) && !qr.get(9, 6));
        // Exactly the codewords fit: data + ECC = raw codewords (7 remainder bits at v6).
        assert_eq!(raw_modules(6) / 8, data_codewords(6) + 16 * 4);
        assert_eq!(raw_modules(6) % 8, 7);
        assert_eq!(QrCode::encode("bvmid2"), Err(Error::Character), "lowercase is not alphanumeric");
        assert_eq!(QrCode::encode(&"A".repeat(400)), Err(Error::TooLong));
    }
}
