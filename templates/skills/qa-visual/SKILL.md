---
name: qa-visual
description: Post-render empirical layout measurement, viewport/device capture, and visual evidence verification for Mobile and Web UI.
---

# Visual & Layout QA — Post-Render Verification

Owns **empirical measurement on real rendered screens** across **Mobile** (`android`, `ios`, `flutter`, `react-native`, `expo`) and **Web** (`web`). Verifies the design rules from `ui-ux-pro-max`, `taste-mobile-app`, and `impeccable` against actual rendered pixels and DOM/tree bounds.

## 1. Measured Geometry & State Matrix

Inspect real rendered views across states (`logged-out`, `populated`, `empty`, `error`, `IME/keyboard open`, `dark mode`, `200% font scale`):
- **Overflow & Clipping**: verify `scrollWidth <= clientWidth` (zero unintended horizontal scroll) and zero clipped labels/buttons near screen edges, notches, or safe areas.
- **Overlap & Alignment**: measure sibling baseline alignment, control gaps, and ensure sticky headers, tab bars, or soft keyboards never occlude interactive controls.
- **Computed Tokens & Contrast**: sample actual rendered colors, font families, and hit-box dimensions against `DESIGN.md` to catch token drift before merge.

## 2. Capture Workflow

- **Mobile**: capture the target state (plus broken `before` screenshot for visual bug fixes) strictly via `mobile-mcp` `mobile_save_screenshot` (honoring `.adb-denylist`); in operator sessions bind and verify via `mobile-proof-qa` (`aaf proof record|verify`).
- **Web**: capture desktop (`1440x900`) and mobile (`390x844`) viewports, check browser console/network errors to zero, and inspect computed geometry directly in the DOM.

## 3. Evidence Integrity

- Report exact measurements (viewport/device id, failing element, measured contrast, artifact SHA-256).
- Never weaken lint thresholds, suppress layout assertions, crop screenshots to hide defects, or attach blank/stale captures.
