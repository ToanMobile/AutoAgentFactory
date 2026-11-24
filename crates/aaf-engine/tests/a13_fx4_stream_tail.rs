#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A13 O13-1: `read_tail_lines` reads only a tail window of the stream log but
//! must stay byte-identical to a whole read of the file's last
//! `MAX_STREAM_LOG_BYTES + 4096` bytes (the whole file when it is within that
//! bound; past it, the partial first line of that suffix is dropped).

use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::path::Path;

use aaf_engine::executors::stream_log::{MAX_STREAM_LOG_BYTES, read_tail_lines};

const WINDOW: usize = 64 * 1024;

/// Reference: a whole read of the bounded suffix, line by line.
fn naive(path: &Path, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    let limit = MAX_STREAM_LOG_BYTES + 4096;
    let mut all = Vec::new();
    fs::File::open(path).unwrap().read_to_end(&mut all).unwrap();
    let buf: &[u8] = if all.len() > limit {
        let suffix = &all[all.len() - limit..];
        suffix
            .iter()
            .position(|&b| b == b'\n')
            .map_or(&[][..], |nl| &suffix[nl + 1..])
    } else {
        &all
    };
    let text = String::from_utf8_lossy(buf);
    let mut ring: VecDeque<String> = VecDeque::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if ring.len() == max_lines {
            ring.pop_front();
        }
        ring.push_back(trimmed.to_string());
    }
    ring.into_iter().collect()
}

fn check(name: &str, bytes: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let streams = dir.path().join("streams");
    fs::create_dir_all(&streams).unwrap();
    let path = streams.join("run_eq.jsonl");
    fs::write(&path, bytes).unwrap();
    for max in [0, 1, 2, 3, 7, 50, 500, 100_000] {
        let fast = read_tail_lines(dir.path(), "run_eq", max).unwrap();
        assert_eq!(fast, naive(&path, max), "case {name} max_lines={max}");
    }
}

/// `n` filler bytes of ASCII lines, so a following byte lands at offset `n`.
fn filler(n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n);
    while v.len() < n {
        let line = format!("{{\"type\":\"event\",\"i\":{}}}\n", v.len());
        v.extend_from_slice(line.as_bytes());
    }
    v.truncate(n);
    v
}

#[test]
fn tail_window_matches_whole_file_read() {
    check("empty", b"");
    check("small", b"{\"a\":1}\n\n  \n{\"b\":2}\r\n{\"c\":3}");
    check("only_blank", b"\n\r\n  \n\t\n");
    check("crlf", &b"{\"x\":1}\r\n".repeat(20_000));
    check(
        "blank_runs_at_end",
        &[filler(3 * WINDOW), b"\n\n\n\r\n   \n".repeat(30_000)].concat(),
    );

    // A multibyte char (and an invalid byte) straddling every offset near the
    // window start, measured from the end of the file.
    let tail = b"\n{\"t\":\"tail\"}\n".repeat(3);
    for shift in 0..8 {
        for (label, glyph) in [
            ("utf8", "\u{1F680}ki\u{1EC3}m".as_bytes()),
            ("invalid", &b"\xF0\x9F\n\xE1\xBB"[..]),
        ] {
            let body = filler(WINDOW + 200);
            let mut bytes = body.clone();
            let at = bytes.len() - WINDOW + shift - tail.len();
            bytes.splice(at..at, glyph.iter().copied());
            bytes.extend_from_slice(&tail);
            check(&format!("{label}_shift{shift}"), &bytes);
        }
    }

    // One line longer than the window (and longer than the 4x growth).
    let long = "z".repeat(5 * WINDOW);
    check(
        "long_line_last",
        &[filler(WINDOW), long.as_bytes().to_vec(), b"\n".to_vec()].concat(),
    );
    check(
        "long_line_then_short",
        &[
            filler(2 * WINDOW),
            long.as_bytes().to_vec(),
            b"\n{\"after\":1}\n".to_vec(),
        ]
        .concat(),
    );

    // Capped log ending in the truncation marker.
    let mut capped = filler(MAX_STREAM_LOG_BYTES - 21);
    capped.push(b'\n');
    capped.extend_from_slice(b"{\"type\":\"truncated\"}\n");
    check("truncated_marker", &capped);

    // File past the read cap: its last cap bytes count, so the lines written
    // after the cap are the ones shown.
    let mut over = filler(MAX_STREAM_LOG_BYTES + 4096 - 10);
    over.extend_from_slice(b"\nCUT-HERE-xyz\n{\"beyond\":1}\n");
    check("over_cap", &over);
}
