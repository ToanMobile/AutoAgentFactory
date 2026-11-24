//! Pairing URL builder, terminal pairing card, and compact pure-Rust QR encoder.
//!
//! Uses a URL fragment (`#token=<token>`) so the bearer token is never sent in
//! HTTP request paths, proxy access logs, or `Referer` headers. The embedded
//! SPA reads `location.hash`, saves it into `sessionStorage`, and clears the
//! address bar immediately via `history.replaceState`.

use std::fmt::Write as _;
use std::net::{IpAddr, UdpSocket};

/// Resolves wildcard bind hosts (`0.0.0.0` or `::`) to the machine's outbound
/// LAN IP (via a non-sending UDP `connect`) so mobile QR pairing links actually
/// connect on the local network. Non-wildcard hosts are returned unchanged.
pub fn display_host_for_pairing(host: &str) -> String {
    let trimmed = host.trim();
    let clean = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    let detect_udp = |bind: &str, target: &str| -> Option<IpAddr> {
        let sock = UdpSocket::bind(bind).ok()?;
        sock.connect(target).ok()?;
        let ip = sock.local_addr().ok()?.ip();
        (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
    };
    let detected = match clean {
        "0.0.0.0" => detect_udp("0.0.0.0:0", "1.1.1.1:80"),
        "::" => detect_udp("[::]:0", "[2606:4700:4700::1111]:80")
            .or_else(|| detect_udp("0.0.0.0:0", "1.1.1.1:80")),
        _ => return trimmed.to_string(),
    };
    detected.map_or_else(|| trimmed.to_string(), |ip| ip.to_string())
}

/// Builds a pairing URL carrying the bearer token solely in the URL fragment.
pub fn pairing_url(host: &str, port: u16, token: &str) -> String {
    let trimmed = host.trim();
    let formatted_host = if trimmed.contains(':') && !trimmed.starts_with('[') {
        format!("[{trimmed}]")
    } else {
        trimmed.to_string()
    };
    format!("http://{formatted_host}:{port}/#token={token}")
}

/// Renders a terminal pairing banner and, when requested, a compact Unicode QR
/// matrix scannable from a phone camera.
pub fn render_pairing_card(url: &str, read_only: bool, lan_mode: bool, include_qr: bool) -> String {
    let mode = if read_only {
        "read-only"
    } else {
        "interactive"
    };
    let bind = if lan_mode {
        "LAN (authenticated)"
    } else {
        "loopback"
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "┌──────────────────────────────────────────────────────────┐"
    );
    let _ = writeln!(out, "│ AAF Web & Mobile Cockpit ({bind} · {mode})");
    let _ = writeln!(out, "│ URL: {url}");
    let _ = writeln!(
        out,
        "│ Security: token stays in #fragment; cleared on page load."
    );
    let _ = writeln!(
        out,
        "└──────────────────────────────────────────────────────────┘"
    );
    if include_qr {
        if let Some(qr) = render_qr_ascii(url) {
            out.push('\n');
            out.push_str(&qr);
        } else {
            let _ = writeln!(
                out,
                "(URL exceeds compact QR capacity of 106 bytes; open the URL above directly)"
            );
        }
    }
    out
}

/// Encodes `payload` as an ISO/IEC 18004 Byte-Mode EC-L QR code (Versions 1..=5,
/// up to 106 UTF-8 bytes) and renders it using crisp UTF-8 half-block characters
/// (`█`, `▀`, `▄`, ` `) with a 2-module light quiet zone.
///
/// Returns `None` when `payload.len() > 106`.
pub fn render_qr_ascii(payload: &str) -> Option<String> {
    let matrix = encode_qr_matrix(payload.as_bytes())?;
    let size = matrix.size;
    let quiet = 2usize;
    let total = size + quiet * 2;
    let is_dark = |r: usize, c: usize| -> bool {
        if r < quiet || c < quiet || r >= quiet + size || c >= quiet + size {
            return false;
        }
        matrix.get(r - quiet, c - quiet)
    };

    // Light modules are rendered with foreground blocks (`█`) and dark modules
    // with terminal background (` `), providing high-contrast light quiet zones
    // on standard dark terminals.
    let mut out = String::new();
    let mut r = 0;
    while r < total {
        for c in 0..total {
            let top_light = !is_dark(r, c);
            let bot_light = if r + 1 < total {
                !is_dark(r + 1, c)
            } else {
                true
            };
            let ch = match (top_light, bot_light) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            };
            out.push(ch);
        }
        out.push('\n');
        r += 2;
    }
    Some(out)
}

#[derive(Debug, Clone)]
pub(crate) struct QrMatrix {
    pub(crate) size: usize,
    modules: Vec<bool>,
    reserved: Vec<bool>,
}

impl QrMatrix {
    fn new(size: usize) -> Self {
        Self {
            size,
            modules: vec![false; size * size],
            reserved: vec![false; size * size],
        }
    }

    pub(crate) fn get(&self, r: usize, c: usize) -> bool {
        self.modules[r * self.size + c]
    }

    fn set_func(&mut self, r: usize, c: usize, dark: bool) {
        let idx = r * self.size + c;
        self.modules[idx] = dark;
        self.reserved[idx] = true;
    }

    fn set_data(&mut self, r: usize, c: usize, dark: bool) {
        self.modules[r * self.size + c] = dark;
    }

    fn is_reserved(&self, r: usize, c: usize) -> bool {
        self.reserved[r * self.size + c]
    }
}

struct VersionSpec {
    version: usize,
    size: usize,
    data_cw: usize,
    ec_cw: usize,
    align_pos: Option<usize>,
}

const VERSIONS: [VersionSpec; 5] = [
    VersionSpec {
        version: 1,
        size: 21,
        data_cw: 19,
        ec_cw: 7,
        align_pos: None,
    },
    VersionSpec {
        version: 2,
        size: 25,
        data_cw: 34,
        ec_cw: 10,
        align_pos: Some(18),
    },
    VersionSpec {
        version: 3,
        size: 29,
        data_cw: 55,
        ec_cw: 15,
        align_pos: Some(22),
    },
    VersionSpec {
        version: 4,
        size: 33,
        data_cw: 80,
        ec_cw: 20,
        align_pos: Some(26),
    },
    VersionSpec {
        version: 5,
        size: 37,
        data_cw: 108,
        ec_cw: 26,
        align_pos: Some(30),
    },
];

pub(crate) fn encode_qr_matrix(data: &[u8]) -> Option<QrMatrix> {
    let spec = VERSIONS.iter().find(|v| data.len() + 2 <= v.data_cw)?;
    let data_codewords = build_byte_mode_codewords(data, spec.data_cw);
    let ec_codewords = rs_remainder(&data_codewords, spec.ec_cw);
    let mut all = Vec::with_capacity(spec.data_cw + spec.ec_cw);
    all.extend_from_slice(&data_codewords);
    all.extend_from_slice(&ec_codewords);

    let mut base = QrMatrix::new(spec.size);
    place_function_patterns(&mut base, spec);
    place_data_bits(&mut base, &all);

    let mut best: Option<(u32, QrMatrix)> = None;
    for mask in 0..8u8 {
        let mut candidate = base.clone();
        apply_mask(&mut candidate, mask);
        place_format_info(&mut candidate, format_info_bits(mask));
        let score = penalty_score(&candidate);
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score < *best_score)
        {
            best = Some((score, candidate));
        }
    }
    best.map(|(_, m)| m)
}

fn build_byte_mode_codewords(data: &[u8], capacity: usize) -> Vec<u8> {
    let mut bits = Vec::with_capacity(capacity * 8);
    push_bits(&mut bits, 0b0100, 4); // Byte mode indicator
    push_bits(&mut bits, data.len() as u32, 8); // 8-bit count for V1..=V9
    for &b in data {
        push_bits(&mut bits, u32::from(b), 8);
    }
    let max_bits = capacity * 8;
    let term = (max_bits - bits.len()).min(4);
    push_bits(&mut bits, 0, term);
    while !bits.len().is_multiple_of(8) {
        bits.push(false);
    }
    let mut codewords = Vec::with_capacity(capacity);
    for chunk in bits.as_chunks::<8>().0 {
        let mut byte = 0u8;
        for &bit in chunk {
            byte = (byte << 1) | u8::from(bit);
        }
        codewords.push(byte);
    }
    let mut pad_idx = 0usize;
    while codewords.len() < capacity {
        codewords.push(if pad_idx.is_multiple_of(2) {
            0xEC
        } else {
            0x11
        });
        pad_idx += 1;
    }
    codewords
}

fn push_bits(out: &mut Vec<bool>, val: u32, count: usize) {
    for shift in (0..count).rev() {
        out.push(((val >> shift) & 1) != 0);
    }
}

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut acc = 0u8;
    for _ in 0..8 {
        if (b & 1) != 0 {
            acc ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1D; // x^8 + x^4 + x^3 + x^2 + 1
        }
        b >>= 1;
    }
    acc
}

fn rs_generator(degree: usize) -> Vec<u8> {
    let mut poly = vec![1u8];
    let mut root = 1u8;
    for _ in 0..degree {
        let mut next = vec![0u8; poly.len() + 1];
        for (i, &c) in poly.iter().enumerate() {
            next[i] ^= c;
            next[i + 1] ^= gf_mul(c, root);
        }
        poly = next;
        root = gf_mul(root, 2);
    }
    poly
}

fn rs_remainder(data: &[u8], ec_len: usize) -> Vec<u8> {
    let generator = rs_generator(ec_len);
    let mut rem = vec![0u8; ec_len];
    for &byte in data {
        let factor = byte ^ rem[0];
        rem.rotate_left(1);
        rem[ec_len - 1] = 0;
        for (r, &g) in rem.iter_mut().zip(generator.iter().skip(1)) {
            *r ^= gf_mul(g, factor);
        }
    }
    rem
}

fn place_function_patterns(m: &mut QrMatrix, spec: &VersionSpec) {
    let s = spec.size;
    place_finder(m, 0, 0);
    place_finder(m, 0, s - 7);
    place_finder(m, s - 7, 0);

    for i in 8..(s - 8) {
        let dark = i.is_multiple_of(2);
        m.set_func(6, i, dark);
        m.set_func(i, 6, dark);
    }

    if let Some(pos) = spec.align_pos {
        for dr in -2isize..=2 {
            for dc in -2isize..=2 {
                let dist = dr.abs().max(dc.abs());
                let dark = dist != 1;
                let r = (pos as isize + dr) as usize;
                let c = (pos as isize + dc) as usize;
                m.set_func(r, c, dark);
            }
        }
    }

    // Reserve format information strips + fixed dark module.
    for i in 0..9 {
        if i != 6 {
            m.set_func(8, i, false);
            m.set_func(i, 8, false);
        }
    }
    for i in (s - 8)..s {
        m.set_func(8, i, false);
    }
    for i in (s - 7)..s {
        m.set_func(i, 8, false);
    }
    m.set_func(4 * spec.version + 9, 8, true);
}

fn place_finder(m: &mut QrMatrix, top: usize, left: usize) {
    let s = m.size as isize;
    for dr in -1isize..=7 {
        for dc in -1isize..=7 {
            let r = top as isize + dr;
            let c = left as isize + dc;
            if r < 0 || c < 0 || r >= s || c >= s {
                continue;
            }
            let in_sep = dr == -1 || dr == 7 || dc == -1 || dc == 7;
            let dark = !in_sep
                && (dr == 0
                    || dr == 6
                    || dc == 0
                    || dc == 6
                    || ((2..=4).contains(&dr) && (2..=4).contains(&dc)));
            m.set_func(r as usize, c as usize, dark);
        }
    }
}

fn place_data_bits(m: &mut QrMatrix, codewords: &[u8]) {
    let s = m.size;
    let total_bits = codewords.len() * 8;
    let mut bit_idx = 0usize;
    let mut col = s - 1;
    let mut upward = true;

    loop {
        if col == 6 {
            col = 5;
        }
        for step in 0..s {
            let row = if upward { s - 1 - step } else { step };
            for offset in 0..2 {
                let c = col - offset;
                if m.is_reserved(row, c) {
                    continue;
                }
                let dark = if bit_idx < total_bits {
                    let byte = codewords[bit_idx / 8];
                    ((byte >> (7 - (bit_idx % 8))) & 1) != 0
                } else {
                    false
                };
                m.set_data(row, c, dark);
                bit_idx += 1;
            }
        }
        if col < 2 {
            break;
        }
        col -= 2;
        upward = !upward;
    }
}

fn mask_bit(mask: u8, r: usize, c: usize) -> bool {
    match mask {
        0 => (r + c).is_multiple_of(2),
        1 => r.is_multiple_of(2),
        2 => c.is_multiple_of(3),
        3 => (r + c).is_multiple_of(3),
        4 => ((r / 2) + (c / 3)).is_multiple_of(2),
        5 => ((r * c) % 2) + ((r * c) % 3) == 0,
        6 => (((r * c) % 2) + ((r * c) % 3)).is_multiple_of(2),
        _ => (((r + c) % 2) + ((r * c) % 3)).is_multiple_of(2),
    }
}

fn apply_mask(m: &mut QrMatrix, mask: u8) {
    for r in 0..m.size {
        for c in 0..m.size {
            if !m.is_reserved(r, c) && mask_bit(mask, r, c) {
                let cur = m.get(r, c);
                m.set_data(r, c, !cur);
            }
        }
    }
}

/// Computes the 15-bit BCH format word for EC Level L (`0b01`) and `mask`.
pub(crate) fn format_info_bits(mask: u8) -> u16 {
    let data = (0b01u16 << 3) | u16::from(mask & 0b111);
    let mut rem = data << 10;
    for shift in (0..=4).rev() {
        if (rem & (1 << (shift + 10))) != 0 {
            rem ^= 0x537 << shift;
        }
    }
    ((data << 10) | rem) ^ 0x5412
}

fn place_format_info(m: &mut QrMatrix, bits: u16) {
    let s = m.size;
    let bit = |k: usize| ((bits >> k) & 1) != 0;

    // Strip 1 around top-left finder (bit 14 at (8,0) .. bit 0 at (0,8)).
    let tl_coords: [(usize, usize); 15] = [
        (0, 8),
        (1, 8),
        (2, 8),
        (3, 8),
        (4, 8),
        (5, 8),
        (7, 8),
        (8, 8),
        (8, 7),
        (8, 5),
        (8, 4),
        (8, 3),
        (8, 2),
        (8, 1),
        (8, 0),
    ];
    for (k, &(r, c)) in tl_coords.iter().enumerate() {
        m.set_func(r, c, bit(k));
    }

    // Strip 2: top-right horizontal (bits 0..=7) and bottom-left vertical (bits 8..=14).
    for k in 0..8 {
        m.set_func(8, s - 1 - k, bit(k));
    }
    for k in 8..15 {
        m.set_func(s - 15 + k, 8, bit(k));
    }
}

fn penalty_score(m: &QrMatrix) -> u32 {
    let s = m.size;
    let mut penalty = 0u32;
    // Rule 1: runs of 5+ identical modules in rows and columns.
    for r in 0..s {
        let mut run = 1u32;
        for c in 1..s {
            if m.get(r, c) == m.get(r, c - 1) {
                run += 1;
            } else {
                if run >= 5 {
                    penalty += run - 2;
                }
                run = 1;
            }
        }
        if run >= 5 {
            penalty += run - 2;
        }
    }
    for c in 0..s {
        let mut run = 1u32;
        for r in 1..s {
            if m.get(r, c) == m.get(r - 1, c) {
                run += 1;
            } else {
                if run >= 5 {
                    penalty += run - 2;
                }
                run = 1;
            }
        }
        if run >= 5 {
            penalty += run - 2;
        }
    }
    // Rule 2: 2x2 uniform blocks.
    for r in 0..(s - 1) {
        for c in 0..(s - 1) {
            let v = m.get(r, c);
            if m.get(r, c + 1) == v && m.get(r + 1, c) == v && m.get(r + 1, c + 1) == v {
                penalty += 3;
            }
        }
    }
    // Rule 4: balance of dark vs light modules.
    let dark = m.modules.iter().filter(|&&b| b).count();
    let total = s * s;
    let pct = (dark * 100) / total;
    let dev = pct.abs_diff(50) / 5;
    penalty + (dev as u32) * 10
}

#[cfg(test)]
mod pairing_host_tests {
    use super::*;

    #[test]
    fn display_host_for_pairing_preserves_explicit_hosts_and_handles_wildcards() {
        assert_eq!(display_host_for_pairing("127.0.0.1"), "127.0.0.1");
        assert_eq!(display_host_for_pairing("localhost"), "localhost");
        assert_eq!(display_host_for_pairing("::1"), "::1");
        assert_eq!(display_host_for_pairing("192.168.1.50"), "192.168.1.50");

        let v4 = display_host_for_pairing("0.0.0.0");
        assert!(!v4.is_empty());
        assert!(v4.parse::<IpAddr>().is_ok(), "unexpected v4 host: {v4}");

        let v6 = display_host_for_pairing("::");
        assert!(!v6.is_empty());
        assert!(v6.parse::<IpAddr>().is_ok(), "unexpected v6 host: {v6}");
    }
}
