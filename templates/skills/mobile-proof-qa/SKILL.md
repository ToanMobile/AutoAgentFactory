---
name: mobile-proof-qa
description: Capture real Android/iOS device UI screenshots strictly via mobile-mcp and verify HMAC-signed proof artifacts in aaf.
---

# Mobile Proof QA (`mobile-mcp` + `aaf proof`)

## 1. Standardize on `mobile-mcp` (`@mobilenext/mobile-mcp`)

- Configure `MOBILEMCP_DISABLE_TELEMETRY=1` in `.mcp.json` for `mobile-mcp` so tool calls and free-text prompts are never sent externally.
- Check `.adb-denylist` in the repo root before any capture; list **both** AVD names (e.g. `PhoneConnect`) and device serials/UDIDs (`emulator-5556`) because `mobile-mcp` often passes AVD names as device identifiers. Never target a denylisted device.
- Save verification PNGs strictly via `mobile_save_screenshot` (`saveTo`: `<repo>/reports/proof.png`). Other capture tools are rejected by default `[proof].allowed_tools`.

## 2. Inside `aaf run` (Sandboxed Agent) vs. Operator Session

- **Inside `aaf run` (`AAF_RUN_ID` set)**: if `mobile-mcp` is available in your session, capture the verified screenshot file (e.g. `reports/proof.png`) and record the exact device id and path in your summary. Do **not** invoke `aaf proof record`, `aaf gate`, `aaf board rework`, or `aaf skills sync` — those are host/operator commands blocked inside `AAF_RUN_ID` / Seatbelt because `receipt.key` is outside the sandbox.
- **Operator / interactive session**: sign and verify the captured PNG against the current `tree_fp` before gating or opening a PR:
  ```sh
  aaf proof record --image reports/proof.png --tool mobile-mcp:mobile_save_screenshot --device <device_id> --task-id <task_id>
  aaf proof verify
  ```
  When `[proof].required = true`, `aaf gate` enforces a valid HMAC-signed proof matching `tree_fp` (PNG magic/`IHDR`/`IDAT`/`IEND`, `min_bytes`, `min_width`/`min_height`, denylist, `allowed_tools = ["mobile-mcp:mobile_save_screenshot"]`) and invalidates receipt reuse if the tree or screenshot changes.

## 3. PR Embedding

- PR bodies generated via `pr_body_with_proof` include verified tool, device, resolution, SHA-256, and `tree_fp`. Never embed blank, stale, or unverified screenshots.
