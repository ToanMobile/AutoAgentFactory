---
name: taste-mobile-app
description: Native mobile art direction and screen architecture for Jetpack Compose, SwiftUI, Flutter, and React Native/Expo.
---

# Taste Mobile App — Native Screen Architecture

Owns **mobile-specific art direction, platform idioms, and 4-state screen architecture** (`android`, `ios`, `flutter`, `react-native`, `expo`). Follows baseline contrast/target/token rules from `ui-ux-pro-max`; skip for Web or Unity games.

## Mobile Architecture Workflow

1. **Brief Gate & Dials**: Infer product intent from the repo first (in interactive sessions only, ask at most 3 domain questions if truly ambiguous; never ask the user to pick colors/fonts). Lock 3 dials (`1–10`): `Expression`, `Motion`, `Density`.
2. **Mobile Token Lock**: Read existing `DESIGN.md`; if none exists, author `MOBILE-DESIGN.md` (palette, type scale, spacing rhythm, motion curves) **before** writing the first screen component.
3. **Content-First Hierarchy**: Give each screen **one clear visual hero/anchor** and task focus. Use typography and whitespace instead of repetitive bordered cards or boilerplate copy (`"Welcome back"`).
4. **Native Layout Mechanics**:
   - Handle edge-to-edge system bars cleanly (Android `WindowInsets` / iOS `safeAreaInsets` / Flutter `SafeArea`).
   - Place primary CTAs inside comfortable bottom thumb-reach.
   - Implement IME/keyboard avoidance so the soft keyboard never obscures active inputs or submit controls.
5. **Mandatory 4 Screen States**:
   - `Loading`: layout-matched skeleton/shimmer (not an uncontextualized spinner alone).
   - `Empty`: domain-specific explanation + single primary next action.
   - `Error`: actionable cause + working Retry trigger.
   - `Populated`: realistic domain formatting, proper ellipsis/wrapping, zero clipped badges.

## Headless / `AAF_RUN_ID` Mode

When `AAF_RUN_ID` is set, **never** ask clarifying questions or wait for confirmation, and never invoke blocked host commands (`aaf gate`, `aaf proof record`, `aaf approve`). Infer product intent, art direction, and the `Expression`/`Motion`/`Density` dials directly from the repo, task spec, and `DESIGN.md`/`MOBILE-DESIGN.md`, implement all 4 screen states and tests autonomously, and finish cleanly on the task branch.
