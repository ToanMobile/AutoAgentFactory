---
name: web-frontend-qa
description: Web frontend TypeScript strictness, WCAG AA accessibility, Core Web Vitals, XSS/CSRF defense, and browser QA.
---

# Web Frontend QA

1. **Type Safety & Rendering Discipline**
   - Preserve strict TypeScript types (`no any` escape hatches), sanitize untrusted HTML/URLs (prevent XSS/open-redirect), and avoid layout shifts or unbounded client rerenders.
   - Guard expensive actions (submit, pay, approve) with immediate disabled/loading state and debounce (`>= 1000ms`).

2. **Accessibility & Core Web Vitals**
   - Use semantic HTML landmarks, keyboard focus visibility, contrast >= 4.5:1, and interactive hit targets >= 44x44px.
   - Protect LCP by sizing hero media, avoiding render-blocking bloat, and checking console/network errors to zero.

3. **Verification**
   - Run the repo's lint, typecheck, unit, and end-to-end suite (and `aaf gate` in operator sessions) before handoff.

## Headless / `AAF_RUN_ID` Mode

When `AAF_RUN_ID` is set, never stop to ask questions or wait for confirmation, and never invoke blocked host commands (`aaf gate`, `aaf approve`). Run the repo's lint, typecheck, unit, and end-to-end commands directly (wrapping heavy builds via `aaf slot -- <cmd>`), fix every finding autonomously, and finish cleanly on the task branch so the factory executes `aaf gate` and cross-audit automatically.
