//! The fixed yes/no questions the auditor answers about a diff.

use aaf_domain::task::ImmutableSpec;

/// One binary question. A "yes" on a blocking item fails the audit; a "yes"
/// on an advisory item is only reported as a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Stable id the auditor must echo back.
    pub id: String,
    pub question: String,
    pub blocking: bool,
}

pub const ID_TEST_BRANCH: &str = "TEST-BRANCH";
pub const ID_HARDCODED: &str = "HARDCODED";
pub const ID_SECRET: &str = "SECRET";
pub const ID_ASSERT: &str = "ASSERT";
pub const ID_GATE_CI: &str = "GATE-CI";
pub const ID_STUB: &str = "STUB";
pub const ID_STORE_ACCOUNT_DELETE: &str = "STORE-ACCOUNT-DELETE";
pub const ID_STORE_SIWA: &str = "STORE-SIWA";
pub const ID_STORE_EXT_PAY: &str = "STORE-EXT-PAY";
pub const ID_STORE_PLACEHOLDER: &str = "STORE-PLACEHOLDER";

/// Id of the item for the `index`-th (0-based) acceptance criterion.
pub fn acceptance_id(index: usize) -> String {
    format!("AC-{}", index + 1)
}

const BLOCKING: [(&str, &str); 6] = [
    (
        ID_TEST_BRANCH,
        "Does production (non-test) code in the diff contain a test-only branch, such as \
         `cfg!(test)`, `if testing`, or an environment check that changes behaviour under test?",
    ),
    (
        ID_HARDCODED,
        "Does the diff hardcode a value, input or expected output whose purpose is to make a \
         test pass instead of implementing the behaviour?",
    ),
    (
        ID_SECRET,
        "Does the diff add a secret or credential (API key, token, password, private key)?",
    ),
    (
        ID_ASSERT,
        "Does the diff delete or weaken an assertion, or skip, ignore or disable a test?",
    ),
    (
        ID_GATE_CI,
        "Does the diff weaken gate or CI configuration (removed checks, lowered thresholds, \
         broader ignore lists, lint suppressions, allowed failures)?",
    ),
    (
        ID_STUB,
        "Does the diff leave a placeholder or stubbed implementation where real behaviour is \
         required?",
    ),
];

const ADVISORY_MOBILE: [(&str, &str); 4] = [
    (
        ID_STORE_ACCOUNT_DELETE,
        "Does the diff add account creation without an in-app account deletion path?",
    ),
    (
        ID_STORE_SIWA,
        "Does the diff add third-party login on iOS without Sign in with Apple?",
    ),
    (
        ID_STORE_EXT_PAY,
        "Does the diff add external payment links for digital goods?",
    ),
    (
        ID_STORE_PLACEHOLDER,
        "Does the diff add placeholder UI text or `localhost` URLs?",
    ),
];

/// Builds the checklist for one audit: the fixed blocking items, one blocking
/// item per acceptance criterion, and the advisory store items when `mobile`.
///
/// Acceptance questions name the criterion by number only: the criterion text
/// is untrusted and stays inside the delimited spec block of the prompt.
pub fn build_checklist(spec: &ImmutableSpec, mobile: bool) -> Vec<Item> {
    let fixed = |(id, question): &(&str, &str), blocking| Item {
        id: (*id).to_string(),
        question: (*question).to_string(),
        blocking,
    };
    let mut items: Vec<Item> = BLOCKING.iter().map(|entry| fixed(entry, true)).collect();
    items.extend((0..spec.acceptance.len()).map(|index| Item {
        id: acceptance_id(index),
        question: format!(
            "Does the diff FAIL to meet acceptance criterion {} of the spec block?",
            index + 1
        ),
        blocking: true,
    }));
    if mobile {
        items.extend(ADVISORY_MOBILE.iter().map(|entry| fixed(entry, false)));
    }
    items
}
