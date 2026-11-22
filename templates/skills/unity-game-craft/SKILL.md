---
name: unity-game-craft
description: Unity 6 C# hot-path zero-GC engineering, deterministic EditMode tests, safe-area layout, and .meta/asmdef hygiene.
---

# Unity Game Craft

## 1. Concrete C# Zero-GC Hot-Path Patterns

Avoid managed heap allocations inside `Update`, `FixedUpdate`, `LateUpdate`, and tight simulation loops:
- **Physics**: use `Physics.RaycastNonAlloc`, `Physics.OverlapSphereNonAlloc`, or `Physics2D.OverlapCircleNonAlloc` with pre-allocated reusable hit buffers instead of `Physics.RaycastAll` / `OverlapSphere`.
- **Coroutines**: cache `WaitForSeconds` / `WaitForFixedUpdate` instances as fields (or a small lookup table by duration) instead of `yield return new WaitForSeconds(...)` per step.
- **Strings**: reuse a pre-sized `StringBuilder` (or `TMP_Text.SetText` char-buffer overloads) for HUD counters/timers instead of per-frame string interpolation (`$"HP: {hp}"`) or concatenation.
- **Collections & Loops**: prefer indexed `for (int i = 0; i < list.Count; i++)` over concrete `List<T>` / arrays; avoid `foreach` over `IEnumerable<T>`/`IList<T>` interfaces (which boxes struct enumerators) and avoid LINQ (`Where`/`Select`/`ToList`) or capturing lambdas in hot paths.
- **References & Lifecycle**: cache `Camera.main` and `GetComponent<T>()` in `Awake`/`Start`; pool frequently spawned projectiles/VFX/UI items; unsubscribe every C# event/delegate and cancel `CancellationToken`s in `OnDisable`/`OnDestroy`.

## 2. Deterministic Tests & Asset Hygiene

- Write deterministic EditMode NUnit tests with fixed PRNG seeds; never claim pass from skipped tests or non-zero exit codes.
- Keep `.meta` files paired with every added, moved, or renamed asset/script, and keep `.asmdef` dependency graphs acyclic.

## 3. Mobile Game UX, Safe Area & Store Safety

- Respect `Screen.safeArea` insets across notched portrait/landscape devices and use 9-slice sprites instead of stretched panels.
- Isolate ads SDK consent, IAP receipt validation boundaries, and Addressables memory budgets behind testable interfaces.
