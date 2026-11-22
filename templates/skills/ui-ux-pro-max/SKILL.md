---
name: ui-ux-pro-max
description: Cross-platform design intelligence for Mobile, Web, and Unity UI/UX — accessibility standards, ergonomic targets, semantic tokens, motion, and anti-slop baseline.
---

# UI/UX Pro Max — Cross-Platform Design Intelligence

Owns **shared UI/UX standards and token foundations** across **Mobile**, **Web**, and **Unity UI** (uGUI / UI Toolkit). Pair with `taste-mobile-app` for native mobile screen architecture, `impeccable` for iterative craft/polish passes, and `qa-visual` for post-render geometric verification.

## Core Standards (Single Source of Truth)

1. **Accessibility (Critical)**: Text contrast `>= 4.5:1` (`>= 3:1` large text), semantic labels/alt text, logical focus order, visible focus rings, and support for `200%` text scaling.
2. **Interaction Ergonomics (Critical)**: Interactive hit targets `>= 48dp` (Android) / `>= 44pt` (iOS) / `>= 44px` (Web) with `>= 8px` gaps; expensive actions (submit, pay, sign, approve) disable immediately on first tap with loading feedback and `>= 1000ms` debounce.
3. **Token & Anti-Slop Floor (High)**: Honor existing `DESIGN.md` / `PRODUCT.md`. Use semantic tokens (`surface`, `onSurface`, `primary`, `danger`) with subtle `3–5%` tinted neutrals and spacing rhythm `4/8/12/16/24/32` — ban bare `#808080`, cliché purple-blue gradients, card-in-card nesting, icon-tile boxes above every heading, and raw emoji as UI chrome.
4. **Layout Performance & Motion (High)**: Reserve media/skeleton bounds to prevent `CLS >= 0.1`; animate `transform`/`opacity` only and respect OS reduced-motion preferences. On Unity UI, use sprite atlases, 9-slice bevels, pressed scale feedback (`0.95`), and disable `raycastTarget` on decorative elements.
5. **Forms, Navigation & Data (Medium)**: Keep visible form labels (never placeholder-only), inline validation preserving input, predictable back-stack navigation (`<= 5` bottom-nav items), and never encode chart/status meaning by color alone.
