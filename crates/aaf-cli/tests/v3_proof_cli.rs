#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Pillar 3 CLI: `aaf proof record|verify|show`.

#[path = "factory_support.rs"]
mod support;

use serde_json::json;
use support::Fixture;

fn crc32(kind: &[u8], data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in kind.iter().chain(data.iter()) {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn push_chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    let len = u32::try_from(data.len()).unwrap();
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&kind, data).to_be_bytes());
}

fn valid_png(width: u32, height: u32, min_bytes: usize) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_chunk(&mut out, *b"IHDR", &ihdr);
    let pad = min_bytes.saturating_sub(out.len() + 24).max(32);
    let mut idat = Vec::with_capacity(pad);
    idat.extend_from_slice(&[0x78, 0x9C]);
    for i in 2..pad {
        idat.push(((i * 37 + 13) % 251) as u8);
    }
    push_chunk(&mut out, *b"IDAT", &idat);
    push_chunk(&mut out, *b"IEND", &[]);
    out
}

#[test]
fn proof_record_verify_and_show_roundtrip_via_cli() {
    let fx = Fixture::new();
    let task_id = fx.add("Verify settings screenshot");
    let img_path = fx.tmp.path().join("capture.png");
    std::fs::write(&img_path, valid_png(1080, 1920, 9000)).unwrap();

    let rec = fx.aaf(&[
        "proof",
        "record",
        "--image",
        img_path.to_str().unwrap(),
        "--device",
        "emulator-5554",
        "--task-id",
        &task_id,
    ]);
    assert_eq!(rec.code, 0, "{}", rec.stderr);
    assert_eq!(
        rec.json["proof"]["source_tool"],
        json!("mobile-mcp:mobile_save_screenshot")
    );
    assert_eq!(rec.json["proof"]["device_serial"], json!("emulator-5554"));

    let ver = fx.aaf(&["proof", "verify"]);
    assert_eq!(ver.code, 0, "{}", ver.stderr);
    assert_eq!(ver.json["status"], json!("CLEAR"));

    let shown = fx.aaf(&["proof", "show"]);
    assert_eq!(shown.code, 0, "{}", shown.stderr);
    assert_eq!(shown.json["proof"]["width"], json!(1080));

    let board_show = fx.show(&task_id);
    assert!(
        board_show["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == json!("proof_recorded")),
        "{board_show:?}"
    );
}

#[test]
fn proof_record_refused_inside_agent_run_and_verify_blocks_when_image_deleted() {
    let fx = Fixture::new();
    let img_path = fx.tmp.path().join("capture.png");
    std::fs::write(&img_path, valid_png(1080, 1920, 9000)).unwrap();

    // Inside an agent run (AAF_RUN_ID set), `aaf proof record` is refused with exit 2.
    let refused = fx.aaf_in(
        &fx.repo,
        &[
            "proof",
            "record",
            "--image",
            img_path.to_str().unwrap(),
            "--device",
            "emulator-5554",
        ],
        &[("AAF_RUN_ID", "run-agent-42")],
    );
    assert_eq!(refused.code, 2, "{}", refused.stderr);
    assert_eq!(refused.json["status"], json!("refused"));

    // Outside agent run, record succeeds; deleting the screenshot makes verify return BLOCKED (exit 2).
    let rec = fx.aaf(&[
        "proof",
        "record",
        "--image",
        img_path.to_str().unwrap(),
        "--device",
        "emulator-5554",
    ]);
    assert_eq!(rec.code, 0, "{}", rec.stderr);

    std::fs::remove_file(&img_path).unwrap();
    let ver_missing = fx.aaf(&["proof", "verify"]);
    assert_eq!(ver_missing.code, 2, "{}", ver_missing.stderr);
    assert_eq!(ver_missing.json["status"], json!("BLOCKED"));

    // Global ~/.config/autofactory/adb-denylist is enforced even when AAF_CONFIG_DIR is unset.
    let img2 = fx.tmp.path().join("capture2.png");
    std::fs::write(&img2, valid_png(1080, 1920, 9000)).unwrap();
    let fake_home = fx.tmp.path().join("fake-home");
    let global_cfg = fake_home.join(".config/autofactory");
    std::fs::create_dir_all(&global_cfg).unwrap();
    std::fs::write(global_cfg.join("adb-denylist"), "PersonalPhone-AVD\n").unwrap();
    let denied = fx.aaf_in(
        &fx.repo,
        &[
            "proof",
            "record",
            "--image",
            img2.to_str().unwrap(),
            "--device",
            "personalphone-avd",
        ],
        &[
            ("HOME", fake_home.to_str().unwrap()),
            ("AAF_CONFIG_DIR", ""),
        ],
    );
    assert_eq!(denied.code, 2, "global denylist under $HOME must exit 2");
    assert_eq!(denied.json["status"], json!("BLOCKED"));
}

#[test]
fn proof_record_enforces_global_allowlist_fail_closed_and_exits_2_on_invalid_png() {
    let fx = Fixture::new();
    let img = fx.tmp.path().join("ok.png");
    std::fs::write(&img, valid_png(1080, 1920, 9000)).unwrap();

    let fake_home = fx.tmp.path().join("home-allow");
    let global_cfg = fake_home.join(".config/autofactory");
    std::fs::create_dir_all(&global_cfg).unwrap();
    let env_home = [
        ("HOME", fake_home.to_str().unwrap()),
        ("AAF_CONFIG_DIR", ""),
    ];

    // Empty global adb-allowlist must fail closed with exit 2 BLOCKED.
    std::fs::write(global_cfg.join("adb-allowlist"), "# empty allowlist\n\n").unwrap();
    let empty_allow = fx.aaf_in(
        &fx.repo,
        &[
            "proof",
            "record",
            "--image",
            img.to_str().unwrap(),
            "--device",
            "emulator-5554",
        ],
        &env_home,
    );
    assert_eq!(empty_allow.code, 2, "{}", empty_allow.stderr);
    assert_eq!(empty_allow.json["status"], json!("BLOCKED"));

    // Non-matching device rejected with exit 2 BLOCKED; listed device accepted.
    std::fs::write(global_cfg.join("adb-allowlist"), "iPhone-16-Sim\n").unwrap();
    let unlisted = fx.aaf_in(
        &fx.repo,
        &[
            "proof",
            "record",
            "--image",
            img.to_str().unwrap(),
            "--device",
            "emulator-5554",
        ],
        &env_home,
    );
    assert_eq!(unlisted.code, 2, "{}", unlisted.stderr);
    assert_eq!(unlisted.json["status"], json!("BLOCKED"));

    let allowed = fx.aaf_in(
        &fx.repo,
        &[
            "proof",
            "record",
            "--image",
            img.to_str().unwrap(),
            "--device",
            "iphone-16-sim",
        ],
        &env_home,
    );
    assert_eq!(allowed.code, 0, "{}", allowed.stderr);

    // Dummy tEXt-padded PNG with tiny IDAT returns exit 2 BLOCKED (not exit 1).
    let mut padded = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&1080u32.to_be_bytes());
    ihdr.extend_from_slice(&1920u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_chunk(&mut padded, *b"IHDR", &ihdr);
    push_chunk(&mut padded, *b"tEXt", &vec![b'Z'; 9000]);
    let mut tiny_idat = vec![0x78, 0x9C];
    tiny_idat.extend_from_slice(&(0u8..16).collect::<Vec<_>>());
    push_chunk(&mut padded, *b"IDAT", &tiny_idat);
    push_chunk(&mut padded, *b"IEND", &[]);
    let padded_path = fx.tmp.path().join("padded.png");
    std::fs::write(&padded_path, padded).unwrap();
    let bad_png = fx.aaf_in(
        &fx.repo,
        &[
            "proof",
            "record",
            "--image",
            padded_path.to_str().unwrap(),
            "--device",
            "iPhone-16-Sim",
        ],
        &env_home,
    );
    assert_eq!(bad_png.code, 2, "{}", bad_png.stderr);
    assert_eq!(bad_png.json["status"], json!("BLOCKED"));
}
