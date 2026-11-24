---
name: impeccable
description: Surface-mode selection, refinement vs. redesign discipline, and iterative UI craft/polish passes for Web and Native Mobile.
---

# Impeccable — Surface Modes & Craft Passes

Owns **surface-mode framing and iterative critique/distillation/polish passes** for **Web** and **Native Mobile** interfaces. Relies on `ui-ux-pro-max` for accessibility/token baselines and `qa-visual` for post-render measurements.

## 1. Choose the Surface Mode First

- **Persuade** (landing, onboarding hero, pricing): clear point of view, strong typographic contrast, decisive primary CTA.
- **Operate** (dashboards, lists, settings, forms): scanability, predictable native patterns, dense clarity over ornament.
- **Read** (docs, articles, logs, changelogs): comfortable line measure, typographic rhythm, effortless skimming.
- **Experience** (galleries, media showcases): let content lead the first viewport while chrome recedes.

## 2. Refinement vs. Redesign

- **Refinement**: preserve existing identity and `DESIGN.md`/`PRODUCT.md` while fixing spacing, alignment, copy, and edge states.
- **Redesign**: treat the legacy layout as anti-reference, replace tokens cleanly in `DESIGN.md`, and commit to one coherent visual direction rather than splitting the difference.

## 3. Focused Craft Passes

- **`shape` / `critique`**: evaluate user flow, visual hierarchy, and information density before coding.
- **`distill` / `quieter` / `clarify`**: strip redundant dividers/chrome, calm noisy widgets, replace vague labels with concise verb-first copy.
- **`harden` / `adapt`**: stress-test i18n expansion, long numbers, narrow viewports, dark mode, and keyboard-only flow.
- **`polish`**: tighten baseline alignment, spacing cadence, and final microcopy.

## 4. Safety & Cost Hygiene

- Never invoke paid image-generation APIs without explicit user approval and cost notice.
- Keep edits strictly inside app source and `DESIGN.md`/`PRODUCT.md`; never alter `.claude/settings.json` or repo hooks.

## 5. Headless / `AAF_RUN_ID` Mode

When `AAF_RUN_ID` is set, never stop to ask questions or wait for confirmation, and never invoke blocked host commands (`aaf gate`, `aaf approve`). Choose the surface mode and refinement vs. redesign scope autonomously from the task spec and existing `DESIGN.md`/`PRODUCT.md`, skip paid external image-generation APIs, and finish cleanly on the task branch.
