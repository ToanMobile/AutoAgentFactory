//! Unit tests of the batched binary-attribute lookups in `gate::changes`.

use super::{binary_set_false, diffed_as_text, parse_check_attr};

/// `check-attr -z` output for several paths and both attributes, in argv
/// order, is read per path (audit a13 P1) by position (R3-L2-1).
#[test]
fn check_attr_output_is_read_per_path() {
    let raw = b"a.bin\0diff\0unset\0a.bin\0working-tree-encoding\0unspecified\0\
                b.txt\0diff\0set\0b.txt\0working-tree-encoding\0unspecified\0\
                c.dat\0diff\0unspecified\0c.dat\0working-tree-encoding\0UTF-16\0\
                d d.x\0diff\0hexdump\0d d.x\0working-tree-encoding\0unset\0\
                e\0diff\0unspecified\0e\0working-tree-encoding\0unspecified\0";
    let attrs = parse_check_attr(raw, 5).unwrap();
    let get = |i: usize| attrs[i].clone().map(Option::unwrap);
    assert_eq!(get(0), ["unset", "unspecified"]);
    assert_eq!(get(1), ["set", "unspecified"]);
    assert_eq!(get(2), ["unspecified", "UTF-16"]);
    assert_eq!(get(3), ["hexdump", "unset"]);
    assert_eq!(get(4), ["unspecified", "unspecified"]);
    assert_eq!(parse_check_attr(b"a\0diff\0", 1), None, "torn triple");
    assert_eq!(parse_check_attr(raw, 6), None, "a path left out");
    let swapped = b"a\0working-tree-encoding\0unset\0a\0diff\0unset\0";
    assert_eq!(
        parse_check_attr(swapped, 1),
        None,
        "attributes out of order"
    );

    let paths = ["a.bin", "b.txt", "c.dat", "d d.x", "e"];
    let text = diffed_as_text(
        &paths,
        |_| Some(raw.to_vec()),
        |key| key == "diff.hexdump.binary",
    );
    let mut text: Vec<_> = text.into_iter().collect();
    text.sort();
    assert_eq!(text, ["b.txt", "c.dat", "d d.x"]);
    let all = diffed_as_text(&paths, |_| None, |_| false);
    assert_eq!(all.len(), paths.len(), "a failed lookup counts as text");
    let one = |_: &[&str]| Some(raw[..57].to_vec());
    let short = diffed_as_text(&[paths[0], "missing"], one, |_| false);
    assert_eq!(short.len(), 2, "a short answer counts as text");
}

/// Git may echo a path in another spelling than it was given (macOS
/// `core.precomposeunicode` echoes NFC for NFD): still that path's.
#[test]
fn check_attr_echo_in_another_spelling_is_still_binary() {
    let echo = "caf\u{e9}.bin\0diff\0unset\0caf\u{e9}.bin\0working-tree-encoding\0unset\0";
    let text = diffed_as_text(&["cafe\u{301}.bin"], |_| Some(echo.into()), |_| false);
    assert!(text.is_empty(), "NFD binary read as text: {text:?}");
}

/// One `check-attr` for many candidates and one `git config` per
/// distinct key, instead of one of each per binary-looking file.
#[test]
fn binary_lookups_are_batched_and_cached() {
    let names: Vec<String> = (0..1_200).map(|i| format!("f{i}.bin")).collect();
    let paths: Vec<&str> = names.iter().map(String::as_str).collect();
    let (mut chunks, mut keys) = (Vec::new(), Vec::new());
    let text = diffed_as_text(
        &paths,
        |chunk| {
            chunks.push(chunk.len());
            let mut raw = Vec::new();
            for (i, path) in chunk.iter().enumerate() {
                let driver = ["unspecified", "custom", "other"][i % 3];
                for (attr, value) in [("diff", driver), ("working-tree-encoding", "unset")] {
                    raw.extend_from_slice(format!("{path}\0{attr}\0{value}\0").as_bytes());
                }
            }
            Some(raw)
        },
        |key| {
            keys.push(key.to_string());
            false
        },
    );
    assert!(text.is_empty());
    assert_eq!(chunks, [500, 500, 200]);
    keys.sort();
    assert_eq!(
        keys,
        [
            "diff.custom.binary",
            "diff.default.binary",
            "diff.other.binary"
        ]
    );
}

/// One `git config` per distinct key (audit a13 O11-1): the `default`
/// driver was asked for twice per binary-looking file.
#[test]
fn each_binary_key_is_asked_once() {
    let mut asked = Vec::new();
    assert!(!binary_set_false("default", |key| {
        asked.push(key.to_string());
        false
    }));
    assert_eq!(asked, ["diff.default.binary"]);

    asked.clear();
    assert!(!binary_set_false("custom", |key| {
        asked.push(key.to_string());
        false
    }));
    assert_eq!(asked, ["diff.custom.binary", "diff.default.binary"]);

    asked.clear();
    assert!(binary_set_false("custom", |key| {
        asked.push(key.to_string());
        key == "diff.default.binary"
    }));
    assert_eq!(asked, ["diff.custom.binary", "diff.default.binary"]);

    asked.clear();
    assert!(binary_set_false("custom", |key| {
        asked.push(key.to_string());
        key == "diff.custom.binary"
    }));
    assert_eq!(asked, ["diff.custom.binary"]);
}
