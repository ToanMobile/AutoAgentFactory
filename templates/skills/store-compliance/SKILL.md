---
name: store-compliance
description: Pre-submission checklist for App Store and Google Play. Use before a release build, a store upload, or a change to manifests, Info.plist, permissions, target SDK, sign-in, payments or account handling in an Android, iOS, Flutter, React Native or Expo app.
---

# Store compliance

Two layers. The gate checks what can be read from the repo with certainty. You check the
rest by reading the code and recording evidence. A clear gate is necessary, not sufficient.

## When to use

- Before tagging a release, building a store artifact, or uploading to either store.
- After touching `AndroidManifest.xml`, Gradle SDK levels, `Info.plist`, `project.pbxproj`,
  `PrivacyInfo.xcprivacy`, Expo `app.json`, permissions, sign-in, purchases or account code.
- When a store review rejected a build: map the rejection to a section below first.

Skip it for a repo with no mobile target; the gate reports `NOT_APPLICABLE` there.

## Headless / `AAF_RUN_ID` Mode

When `AAF_RUN_ID` is set, do **not** run `aaf gate` or `aaf approve` (blocked inside the sandbox; the factory runs `aaf gate` automatically after your turn), and do **not** stop to ask questions or wait for human decisions. Inspect manifests, plists, privacy manifests, and source code directly, fix every actionable source-level CRITICAL/WARN issue autonomously, verify with the project's local build/test commands, and list any external store-console items (Play Console declarations, demo account, regional billing enrollment) in your final summary without blocking.

## Run the gate (Operator / Interactive Session)

```sh
aaf gate --release          # human-readable
aaf gate --release --json   # one JSON object on stdout
```

| Exit | Meaning | What to do |
|---|---|---|
| 0 | `CLEAR` | Continue with the judgement checks below. |
| 2 | `BLOCKED` or `UNVERIFIED` | Fix every CRITICAL, or give the gate a test command to run. |
| 1 | Runtime, config or I/O error | Fix the tool problem. It says nothing about the app. |

Read `store_compliance` in the receipt:

- `status`: `CLEAR`, `BLOCKED`, `SKIPPED` (turned off in config) or `NOT_APPLICABLE`.
  `SKIPPED` is not a pass: do not submit on it.
- `findings[]`: `rule`, `severity`, `file`, `line`, `message`, `fix`. Any `critical` blocks.
- `unresolved[]`: values the scanner could not read statically (a Gradle variable, a
  `$(VAR)` in a plist, `app.config.js`). Resolve each by hand: find the real value in the
  build output or the merged manifest and check it against the same rule.

Rules for working with findings:

- Fix the cause in the source file. Never edit generated output, never turn the scan off,
  never lower `gate.min_android_target_sdk` to get a pass.
- Re-run the gate after each fix. Report the before and after status.
- A finding you believe is wrong is reported to the human with the file and line. It is
  not suppressed silently.

## Rule ids

### CRITICAL (block the release)

**AND-001 — `android:debuggable="true"` in a main or release manifest.**
A debuggable build exposes app data and is refused by Google Play. Delete the attribute;
the build type sets it (`debug` on, `release` off). If a debug-only tool needs it, move
that to `src/debug/AndroidManifest.xml`.

**AND-002 — component with an `<intent-filter>` and no `android:exported`.**
Current Android refuses to install such an app, and an implicit export is a security hole.
Add the attribute to every activity, service, receiver and provider that has a filter:
`true` only when other apps must start it (launcher activity, deep links, share targets),
otherwise `false`. An exported component must validate everything in its incoming intent.

**AND-003 — `targetSdk` below `gate.min_android_target_sdk` (default 36).**
Google Play requires target API level 36 for new apps and updates since 2026-08-31
(Wear OS and Automotive: 35, TV and XR: 34). Raise `targetSdk` where it is defined
(`build.gradle(.kts)`, `gradle/libs.versions.toml`, `gradle.properties`, Expo `app.json`),
then work through the behaviour changes of each API level you crossed and test on a device
at the new level. Raising the number without that testing trades a store block for crashes.
If the app is a Wear OS, Automotive, TV or XR app, the lower level is set in
`.autofactory/config.toml` by a human, with the form factor stated in the commit.

**IOS-001 — empty or placeholder `NS…UsageDescription`.**
Every protected resource the app (or any linked SDK) touches needs a purpose string that
tells the user what is accessed and why, in the app's own terms. Bad: `TODO`, `Camera`,
`This app needs access`. Good: "Scan the barcode on a parcel to add it to your shipments".
Fix it at the source the finding names (`Info.plist`, `INFOPLIST_KEY_*` build settings,
`expo.ios.infoPlist`) and in every localisation. If the app does not use the resource,
remove the key and the code or SDK that requests it instead.

**IOS-002 — required-reason API used without `PrivacyInfo.xcprivacy`.**
Apple requires a privacy manifest declaring an approved reason for each required-reason
API category in use. Add `PrivacyInfo.xcprivacy` to the app target and list each category
under `NSPrivacyAccessedAPITypes` with the reason code that matches the real use:

| API seen in code | Category |
|---|---|
| `UserDefaults`, `NSUserDefaults` | `NSPrivacyAccessedAPICategoryUserDefaults` |
| `fileModificationDate`, `contentModificationDateKey`, `creationDateKey` | `NSPrivacyAccessedAPICategoryFileTimestamp` |
| `systemUptime`, `mach_absolute_time` | `NSPrivacyAccessedAPICategorySystemBootTime` |
| `volumeAvailableCapacity…`, `systemFreeSize` | `NSPrivacyAccessedAPICategoryDiskSpace` |
| `activeInputModes` | `NSPrivacyAccessedAPICategoryActiveKeyboards` |

Kotlin Multiplatform counts too: code under `src/iosMain/` (and `appleMain`, `iosArm64Main`,
…) reaches the same APIs through `platform.*` under their Objective-C names
(`NSUserDefaults`, `NSFileModificationDate` also as the string key `"NSFileModificationDate"`,
`NSURLVolumeAvailableCapacityKey`, `platform.posix.stat(…)`, `NSProcessInfo…systemUptime`).
The finding is a CRITICAL when an Xcode project in the repo links that module's framework
(`<module>/build/xcode-frameworks`), otherwise an unresolved WARN. The manifest goes into the
app target, or into the module's iOS resources.

Take reason codes from Apple's current list; do not invent one or pick the first that
compiles. The manifest must also be honest about tracking and collected data types.

### WARN (do not block; each needs a decision you write down)

**AND-101 — `usesCleartextTraffic="true"` without a `networkSecurityConfig`.**
Use HTTPS. If one host really needs HTTP (a local device, a dev server), allow only that
host in a network security config and keep it out of release where possible.

**AND-102 — sensitive permission** (`READ_SMS`, `READ_CALL_LOG`,
`MANAGE_EXTERNAL_STORAGE`, `REQUEST_INSTALL_PACKAGES`, `QUERY_ALL_PACKAGES`,
`ACCESS_BACKGROUND_LOCATION`). Google Play restricts each to apps whose core function
needs it and asks for a declaration in Play Console. Prefer the narrower alternative
(SMS Retriever or user consent API, the photo picker or Storage Access Framework,
`<queries>` entries, foreground location). If it stays, the human must file the declaration;
say so in your report. Check merged manifests too: a library can add the permission.

**AND-103 — `allowBackup="true"`.** Backups can carry tokens and private data to another
device. Set it to `false`, or keep it and exclude secrets with backup/data-extraction rules.

**IOS-101 — privacy manifest present but missing a category for an API in use.**
Add the category from the table above with its real reason.

**IOS-102 — usage description shorter than 10 characters.** Too short to explain a
purpose. Rewrite as for IOS-001.

## Judgement checks (the gate cannot decide these)

For each item: search, read the code path, then record `applies / does not apply`,
the evidence (file and line, or a screen you reached), and the verdict. "Not found" needs
the search you ran. Policies change: when a verdict depends on an exception or a regional
programme, quote the current policy text to the human instead of trusting memory.

### 1. Account deletion

Both stores require that an app which lets people create an account also lets them start
deleting that account, and its data, from inside the app. Google Play additionally asks
for a web link where deletion can be requested without the app, declared in the Data
safety form.

- Applies if: any sign-up, registration or first-login-creates-account flow exists.
- Evidence to find: a reachable settings entry (not a hidden URL); the client call and the
  server endpoint that deletes rather than deactivates; revocation of third-party tokens
  (Sign in with Apple tokens must be revoked on deletion); what is retained and why.
- Fails when: the only route is "email support", the control merely signs out or
  disables, or deletion is only possible on a website for an account created in the app.

### 2. Sign in with Apple (iOS)

An app that uses a third-party or social login for its primary account must also offer
an equivalent login option that limits data collection to name and email, lets the user
keep the email private and does not track them. Sign in with Apple satisfies this.

- Applies if: the iOS app offers Google, Facebook or a similar third-party login.
- Does not apply if: the app uses only its own account system, or one of the exceptions
  in the current guideline (for example a client for one specific third-party service, or
  organisation-issued accounts). Confirm the exception against the current text.
- Evidence to find: the list of auth providers in the login screen and SDK config; the
  Sign in with Apple entitlement and button on the same screen with equal prominence;
  handling of the private relay email and of a name delivered only on first sign-in.

### 3. Payments

Digital content, subscriptions and features consumed inside the app are sold through the
store's own billing (StoreKit in-app purchase, Google Play Billing). Physical goods and
services consumed outside the app use other payment methods.

- Applies if: the app unlocks anything after payment, or shows prices or plans.
- Evidence to find: every purchase entry point; which SDK completes it; any web checkout,
  payment-provider SDK, link or text that sends users elsewhere to pay for digital goods;
  a restore-purchases control; subscription terms and price shown before purchase.
- External purchase links and alternative billing exist only under specific regional
  programmes with their own entitlements and enrolment. Do not assume one applies: report
  the storefront countries and what you found, and let the human decide.

### 4. Placeholder and incomplete content

Reviewers reject builds that look unfinished.

- Evidence to find: `lorem ipsum`, `TODO`, `Coming soon`, sample or test data in shipped
  screens and strings; default template icons, splash screens and app names; dead buttons
  and empty screens; links that 404; debug menus, test accounts or staging endpoints
  reachable in release; store-listing text and screenshots that show features absent from
  the build.
- If the app needs a login, the submission needs a working demo account for the reviewer.
  Report whether one exists; never commit its password.

## Report

End with a short table: gate status before and after, each rule id fixed (file, line),
each WARN with its decision, each unresolved value with how it was resolved, and the four
judgement checks with verdict and evidence. List anything that needs the human (console
declarations, demo account, regional payment decisions) separately and plainly.
