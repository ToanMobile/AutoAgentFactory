//! Built-in production SOP workflow blueprints.

use aaf_domain::gate::GateMode;
use aaf_domain::sop::{SopSpec, SopStage, StageKind};

fn stage(id: &str, title: &str, kind: StageKind, depends_on: &[&str]) -> SopStage {
    SopStage {
        id: id.to_string(),
        title: title.to_string(),
        kind,
        depends_on: depends_on.iter().map(|s| (*s).to_string()).collect(),
        on_fail_goto: None,
        artifacts: Vec::new(),
    }
}

fn with_rework(mut stage: SopStage, target: &str) -> SopStage {
    stage.on_fail_goto = Some(target.to_string());
    stage
}

fn with_artifacts(mut stage: SopStage, artifacts: &[&str]) -> SopStage {
    stage.artifacts = artifacts.iter().map(|s| (*s).to_string()).collect();
    stage
}

fn agent(role: &str, read_only: bool, prompt: &str) -> StageKind {
    StageKind::Agent {
        role: role.to_string(),
        executor: None,
        model: None,
        read_only,
        prompt: prompt.to_string(),
    }
}

fn script(command: &str, heavy: bool, timeout_secs: u64) -> StageKind {
    StageKind::Script {
        command: command.to_string(),
        heavy,
        timeout_secs,
    }
}

fn human_gate(prompt: &str) -> StageKind {
    StageKind::HumanGate {
        prompt: prompt.to_string(),
    }
}

fn backend_sop() -> SopSpec {
    SopSpec {
        name: "backend".to_string(),
        version: 1,
        description: "Backend service delivery: read-only plan, implementation, scoped gate, independent review with rework loop, full gate, and human sign-off.".to_string(),
        profile: "backend".to_string(),
        max_reworks: 3,
        stages: vec![
            with_artifacts(
                stage(
                    "plan",
                    "Clarify & Plan",
                    agent(
                        "architect",
                        true,
                        "Analyze requirements, API contracts, security boundaries, and rollback impact; produce a concise implementation plan without editing code.",
                    ),
                    &[],
                ),
                &["plan.md"],
            ),
            with_artifacts(
                stage(
                    "implement",
                    "Implement & Unit Test",
                    agent(
                        "implementer",
                        false,
                        "Implement the approved plan with deterministic unit tests covering both allow and deny branches.",
                    ),
                    &["plan"],
                ),
                &["diff.patch"],
            ),
            with_artifacts(
                stage(
                    "gate_scoped",
                    "Scoped Verification Gate",
                    StageKind::Gate {
                        mode: GateMode::Scoped,
                    },
                    &["implement"],
                ),
                &["gate-scoped.json"],
            ),
            with_artifacts(
                with_rework(
                    stage(
                        "review",
                        "Independent Review",
                        agent(
                            "reviewer",
                            true,
                            "Audit diff against plan, error handling, security invariants, and test coverage without editing files.",
                        ),
                        &["gate_scoped"],
                    ),
                    "implement",
                ),
                &["review.md"],
            ),
            with_artifacts(
                stage(
                    "gate_full",
                    "Full Verification Gate",
                    StageKind::Gate {
                        mode: GateMode::Full,
                    },
                    &["review"],
                ),
                &["gate-full.json"],
            ),
            stage(
                "signoff",
                "Human Sign-off",
                human_gate(
                    "Review the implementation, independent audit, and full gate receipt before approving release.",
                ),
                &["gate_full"],
            ),
        ],
    }
}

fn frontend_sop() -> SopSpec {
    SopSpec {
        name: "frontend".to_string(),
        version: 1,
        description: "Frontend web delivery: plan, implementation, lint/build script, visual & QA review with rework loop, full gate, and human sign-off.".to_string(),
        profile: "web".to_string(),
        max_reworks: 3,
        stages: vec![
            with_artifacts(
                stage(
                    "plan",
                    "UI & Component Plan",
                    agent(
                        "architect",
                        true,
                        "Define component boundaries, responsive states, accessibility checks, and test plan without modifying code.",
                    ),
                    &[],
                ),
                &["plan.md"],
            ),
            with_artifacts(
                stage(
                    "implement",
                    "Implement UI Changes",
                    agent(
                        "implementer",
                        false,
                        "Implement UI components, states, and tests according to the plan.",
                    ),
                    &["plan"],
                ),
                &["diff.patch"],
            ),
            with_artifacts(
                stage(
                    "lint_build",
                    "Lint & Production Build",
                    script("npm run lint && npm run build", false, 300),
                    &["implement"],
                ),
                &["build.log"],
            ),
            with_artifacts(
                with_rework(
                    stage(
                        "review",
                        "Visual & QA Review",
                        agent(
                            "reviewer",
                            true,
                            "Verify responsive layout, accessibility states, error handling, and visual polish without modifying files.",
                        ),
                        &["lint_build"],
                    ),
                    "implement",
                ),
                &["qa-review.md"],
            ),
            with_artifacts(
                stage(
                    "gate_full",
                    "Full Verification Gate",
                    StageKind::Gate {
                        mode: GateMode::Full,
                    },
                    &["review"],
                ),
                &["gate-full.json"],
            ),
            stage(
                "signoff",
                "Human Sign-off",
                human_gate(
                    "Confirm UI screenshots, QA review findings, and full gate receipt before approving.",
                ),
                &["gate_full"],
            ),
        ],
    }
}

fn mobile_android_sop() -> SopSpec {
    SopSpec {
        name: "mobile-android".to_string(),
        version: 1,
        description: "Android delivery: plan, implementation, store compliance & scoped gate, independent review with rework loop, release gate, and human sign-off.".to_string(),
        profile: "android".to_string(),
        max_reworks: 3,
        stages: vec![
            with_artifacts(
                stage(
                    "plan",
                    "Architecture & Compliance Plan",
                    agent(
                        "architect",
                        true,
                        "Plan Android changes, permissions, Play Store policy impact, and verification matrix without editing code.",
                    ),
                    &[],
                ),
                &["plan.md"],
            ),
            with_artifacts(
                stage(
                    "implement",
                    "Implement Android Feature",
                    agent(
                        "implementer",
                        false,
                        "Implement the Android feature and deterministic tests following the plan.",
                    ),
                    &["plan"],
                ),
                &["diff.patch"],
            ),
            with_artifacts(
                stage(
                    "gate_scoped",
                    "Store Compliance & Scoped Gate",
                    StageKind::Gate {
                        mode: GateMode::Scoped,
                    },
                    &["implement"],
                ),
                &["gate-scoped.json"],
            ),
            with_artifacts(
                with_rework(
                    stage(
                        "review",
                        "Independent Review",
                        agent(
                            "reviewer",
                            true,
                            "Audit Android diff, lifecycle safety, permissions, and device proof requirements without editing files.",
                        ),
                        &["gate_scoped"],
                    ),
                    "implement",
                ),
                &["review.md"],
            ),
            with_artifacts(
                stage(
                    "gate_release",
                    "Release Gate (Full)",
                    StageKind::Gate {
                        mode: GateMode::Full,
                    },
                    &["review"],
                ),
                &["gate-release.json"],
            ),
            stage(
                "signoff",
                "Human Sign-off",
                human_gate(
                    "Verify device proof, store compliance, and full gate receipt before approving Android release.",
                ),
                &["gate_release"],
            ),
        ],
    }
}

fn unity_game_sop() -> SopSpec {
    SopSpec {
        name: "unity-game".to_string(),
        version: 1,
        description: "Unity game delivery: plan, implementation, heavy EditMode test script, independent review with rework loop, full gate, and human sign-off.".to_string(),
        profile: "unity".to_string(),
        max_reworks: 3,
        stages: vec![
            with_artifacts(
                stage(
                    "plan",
                    "Gameplay & Scene Plan",
                    agent(
                        "architect",
                        true,
                        "Outline Unity scripts, ScriptableObjects, scene impact, and EditMode test plan without modifying assets.",
                    ),
                    &[],
                ),
                &["plan.md"],
            ),
            with_artifacts(
                stage(
                    "implement",
                    "Implement Unity Changes",
                    agent(
                        "implementer",
                        false,
                        "Implement C# gameplay logic and EditMode tests according to the plan.",
                    ),
                    &["plan"],
                ),
                &["diff.patch"],
            ),
            with_artifacts(
                stage(
                    "editmode_tests",
                    "Heavy Unity EditMode Tests",
                    script("unity-batchmode -runTests -testPlatform EditMode", true, 900),
                    &["implement"],
                ),
                &["editmode-results.xml"],
            ),
            with_artifacts(
                with_rework(
                    stage(
                        "review",
                        "Independent Review",
                        agent(
                            "reviewer",
                            true,
                            "Review C# diff, allocations on hot paths, serialization stability, and EditMode test results.",
                        ),
                        &["editmode_tests"],
                    ),
                    "implement",
                ),
                &["review.md"],
            ),
            with_artifacts(
                stage(
                    "gate_full",
                    "Full Verification Gate",
                    StageKind::Gate {
                        mode: GateMode::Full,
                    },
                    &["review"],
                ),
                &["gate-full.json"],
            ),
            stage(
                "signoff",
                "Human Sign-off",
                human_gate(
                    "Inspect EditMode results, review notes, and full gate receipt before approving.",
                ),
                &["gate_full"],
            ),
        ],
    }
}

fn bugfix_sop() -> SopSpec {
    SopSpec {
        name: "bugfix".to_string(),
        version: 1,
        description: "Root-cause bugfix: read-only analysis, regression test + fix, scoped gate with rework loop, review, and human sign-off.".to_string(),
        profile: "auto".to_string(),
        max_reworks: 3,
        stages: vec![
            with_artifacts(
                stage(
                    "analyze",
                    "Root-Cause Analysis",
                    agent(
                        "architect",
                        true,
                        "Reproduce the bug, isolate the exact root cause, and specify a failing regression oracle without editing code.",
                    ),
                    &[],
                ),
                &["rca.md"],
            ),
            with_artifacts(
                stage(
                    "fix",
                    "Regression Test & Fix",
                    agent(
                        "implementer",
                        false,
                        "Add the paired RED->GREEN regression test and apply the minimal fix targeting the root cause.",
                    ),
                    &["analyze"],
                ),
                &["fix.patch"],
            ),
            with_artifacts(
                with_rework(
                    stage(
                        "gate_scoped",
                        "Scoped Gate",
                        StageKind::Gate {
                            mode: GateMode::Scoped,
                        },
                        &["fix"],
                    ),
                    "fix",
                ),
                &["gate-scoped.json"],
            ),
            with_artifacts(
                stage(
                    "review",
                    "Independent Review",
                    agent(
                        "reviewer",
                        true,
                        "Verify the fix addresses the root cause without regressions and that the regression oracle is non-tautological.",
                    ),
                    &["gate_scoped"],
                ),
                &["review.md"],
            ),
            stage(
                "signoff",
                "Human Sign-off",
                human_gate(
                    "Confirm root-cause analysis, regression test, and review verdict before approving the bugfix.",
                ),
                &["review"],
            ),
        ],
    }
}

/// Returns all 5 built-in production SOP blueprints.
pub fn builtin_sops() -> Vec<SopSpec> {
    vec![
        backend_sop(),
        frontend_sop(),
        mobile_android_sop(),
        unity_game_sop(),
        bugfix_sop(),
    ]
}

/// Looks up a built-in SOP blueprint by exact name.
pub fn find_builtin(name: &str) -> Option<SopSpec> {
    builtin_sops().into_iter().find(|s| s.name == name)
}

/// Serializes an [`SopSpec`] into pretty TOML suitable for `.autofactory/sops/<name>.toml`.
pub fn render_toml(spec: &SopSpec) -> Result<String, String> {
    toml::to_string_pretty(spec).map_err(|e| format!("serialize SOP TOML: {e}"))
}
