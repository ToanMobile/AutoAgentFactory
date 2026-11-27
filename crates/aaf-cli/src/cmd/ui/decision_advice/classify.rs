//! Deterministic decision-item classification, input hashing, fast-path policy,
//! and monotonic safety hardening for Tier-3 AI decision advice.

use aaf_domain::task::Task;
use serde::{Deserialize, Serialize};

use super::store::{AdviceItem, AdviceOption};

pub const MAX_ITEMS: usize = 16;
pub const MAX_OPTIONS_PER_ITEM: usize = 5;
pub const MAX_QUESTION_CHARS: usize = 600;
pub const MAX_HINT_CHARS: usize = 400;
pub const MAX_OPT_TITLE_CHARS: usize = 160;
pub const MAX_OPT_DESC_CHARS: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    NeedsInput,
    NeedsAccess,
    NeedsDevice,
    ScopeChoice,
    VerifyOnBase,
    Unknown,
}

impl DecisionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NeedsInput => "needs_input",
            Self::NeedsAccess => "needs_access",
            Self::NeedsDevice => "needs_device",
            Self::ScopeChoice => "scope_choice",
            Self::VerifyOnBase => "verify_on_base",
            Self::Unknown => "unknown",
        }
    }

    pub fn requires_human_input(self) -> bool {
        matches!(
            self,
            Self::NeedsInput | Self::NeedsAccess | Self::NeedsDevice
        )
    }

    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "needs_input" => Self::NeedsInput,
            "needs_access" => Self::NeedsAccess,
            "needs_device" => Self::NeedsDevice,
            "scope_choice" => Self::ScopeChoice,
            "verify_on_base" => Self::VerifyOnBase,
            _ => Self::Unknown,
        }
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// Deterministic server-side classifier for one decision item or blocker reason.
/// Ordered from strictest human-input blockers to actionable scope/verify items.
pub fn classify_item(raw: &str) -> DecisionKind {
    let lower = raw.to_ascii_lowercase();
    if contains_any(
        &lower,
        &[
            "cấp quyền",
            "quyền đọc",
            "cung cấp secret",
            "supabase",
            "api key",
            "access token",
            "credential",
            "keychain",
            "oauth",
            "secret",
        ],
    ) {
        return DecisionKind::NeedsAccess;
    }
    if contains_any(
        &lower,
        &[
            "xe thật",
            "máy thật",
            "người lái",
            "đo trên xe",
            "ihu629g",
            "thiết bị thật",
            "real device",
            "real car",
            "physical device",
        ],
    ) {
        return DecisionKind::NeedsDevice;
    }
    if contains_any(
        &lower,
        &[
            "blocker:",
            "please paste",
            "paste its text",
            "what is ",
            "which file",
            "cung cấp thêm",
            "dán nội dung",
            "thiếu mô tả",
        ],
    ) {
        return DecisionKind::NeedsInput;
    }
    if contains_any(
        &lower,
        &[
            "ngoài scope",
            "out of scope",
            "trùng lane",
            "chỉ tích ac-",
            "chỉ tích ac ",
            "hay hoãn",
            " hay ",
            " or ",
            "phương án",
            "(1)",
        ],
    ) {
        return DecisionKind::ScopeChoice;
    }
    if contains_any(
        &lower,
        &[
            "grep marker",
            "trên base",
            "xác nhận trên",
            "verify on base",
            "kiểm tra trên main",
            "chưa xác minh",
        ],
    ) {
        return DecisionKind::VerifyOnBase;
    }
    DecisionKind::Unknown
}

/// Computes the canonical SHA-256 input fingerprint binding cached advice to the
/// task's spec, blocked reason, and open questions.
pub fn compute_input_hash(task: &Task, blocked_text: &str, open_questions: &[String]) -> String {
    let mut buf = String::with_capacity(512);
    buf.push_str(&task.id);
    buf.push('\n');
    buf.push_str(task.state.as_str());
    buf.push('\n');
    buf.push_str(&task.title);
    buf.push('\n');
    buf.push_str(&task.spec.description);
    buf.push('\n');
    for ac in &task.spec.acceptance {
        buf.push_str(ac);
        buf.push('\n');
    }
    buf.push_str(blocked_text.trim());
    buf.push('\n');
    for q in open_questions {
        buf.push_str(q.trim());
        buf.push('\n');
    }
    aaf_fp::sha256_hex(buf.as_bytes())
}

pub fn sanitize_text(raw: &str, max_chars: usize) -> String {
    raw.chars()
        .filter(|&c| !c.is_control() || c == ' ')
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

/// Returns `Some(items)` when every decision item is deterministically a human
/// access/device blocker — avoids burning LLM tokens on tasks requiring physical
/// action or external credentials.
pub fn try_deterministic_fast_path(raw_items: &[String]) -> Option<Vec<AdviceItem>> {
    if raw_items.is_empty() {
        return None;
    }
    let kinds: Vec<DecisionKind> = raw_items.iter().map(|q| classify_item(q)).collect();
    if !kinds
        .iter()
        .all(|k| matches!(k, DecisionKind::NeedsAccess | DecisionKind::NeedsDevice))
    {
        return None;
    }
    let items = raw_items
        .iter()
        .zip(kinds)
        .take(MAX_ITEMS)
        .map(|(q, kind)| {
            let hint = match kind {
                DecisionKind::NeedsAccess => "Mục này cần bạn cấp quyền truy cập / cấu hình secret hoặc biến môi trường cục bộ trước khi gỡ chặn. Nhập xác nhận hoặc cấu hình đã hoàn tất vào mục Quyết định khác rồi mới gửi.",
                DecisionKind::NeedsDevice => "Mục này cần đo/kiểm tra thực tế trên thiết bị hoặc xe thật. Nhập kết quả đo thực tế vào mục Quyết định khác để gỡ chặn, hoặc chọn làm phần tự động trước nếu khả thi.",
                _ => unreachable!(),
            };
            let options = match kind {
                DecisionKind::NeedsDevice => vec![AdviceOption {
                    num: "1".into(),
                    title: "Hoãn bước đo thiết bị thật, làm phần mã tự động trước".into(),
                    desc: "Chỉ chọn nếu task còn phần mã/test có thể hoàn tất độc lập mà không cần dữ liệu đo ngay.".into(),
                    recommended: false,
                }],
                _ => Vec::new(),
            };
            AdviceItem {
                question: sanitize_text(q, MAX_QUESTION_CHARS),
                kind: kind.as_str().into(),
                requires_human_input: true,
                hint: hint.into(),
                options,
            }
        })
        .collect();
    Some(items)
}

fn looks_like_ac_shortcut(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    contains_any(
        &lower,
        &[
            "chỉ tích ac-",
            "chỉ tích ac ",
            "bỏ qua test",
            "skip test",
            "tắt gate",
            "disable gate",
            "bỏ qua ac",
            "drop ac",
            "skip ac",
        ],
    )
}

fn cites_repo_path(hint: &str) -> bool {
    hint.contains('/')
        || contains_any(
            hint,
            &[
                ".rs", ".kt", ".java", ".ts", ".js", ".py", ".gradle", ".xml", ".json",
            ],
        )
}

/// Hardens model-produced advice items against prompt injection, shortcut bias,
/// and safety downgrade. Server-owned `needs_access` / `needs_device` /
/// `needs_input` classifications are strictly monotonic (model can only tighten).
pub fn merge_and_harden_items(
    raw_items: &[String],
    model_items: Vec<AdviceItem>,
) -> Vec<AdviceItem> {
    let mut out = Vec::with_capacity(raw_items.len().min(MAX_ITEMS));
    for (idx, raw_q) in raw_items.iter().take(MAX_ITEMS).enumerate() {
        let server_kind = classify_item(raw_q);
        let model = model_items.get(idx);
        let model_kind = model
            .map(|m| DecisionKind::parse(&m.kind))
            .unwrap_or(DecisionKind::Unknown);

        // Monotonic kind: server human-input kinds are sticky; model may tighten.
        let final_kind = if server_kind.requires_human_input() {
            server_kind
        } else if model_kind.requires_human_input() {
            model_kind
        } else if model_kind != DecisionKind::Unknown {
            model_kind
        } else {
            server_kind
        };

        let mut requires_input =
            final_kind.requires_human_input() || model.is_some_and(|m| m.requires_human_input);

        let hint = model
            .map(|m| sanitize_text(&m.hint, MAX_HINT_CHARS))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default_hint_for_kind(final_kind).to_string());

        let question = model
            .map(|m| sanitize_text(&m.question, MAX_QUESTION_CHARS))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| sanitize_text(raw_q, MAX_QUESTION_CHARS));

        let mut options: Vec<AdviceOption> = model
            .map(|m| {
                m.options
                    .iter()
                    .take(MAX_OPTIONS_PER_ITEM)
                    .enumerate()
                    .filter_map(|(oi, o)| {
                        let title = sanitize_text(&o.title, MAX_OPT_TITLE_CHARS);
                        if title.is_empty() {
                            return None;
                        }
                        let num = sanitize_text(&o.num, 8);
                        Some(AdviceOption {
                            num: if num.is_empty() {
                                (oi + 1).to_string()
                            } else {
                                num
                            },
                            title,
                            desc: sanitize_text(&o.desc, MAX_OPT_DESC_CHARS),
                            recommended: o.recommended,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Guard: if a model recommends an AC/test-dropping shortcut without file evidence,
        // strip the recommendation and require explicit human input.
        let mut rec_seen = false;
        for opt in &mut options {
            if opt.recommended {
                let opt_text = format!("{} {}", opt.title, opt.desc);
                if requires_input || (looks_like_ac_shortcut(&opt_text) && !cites_repo_path(&hint))
                {
                    opt.recommended = false;
                    if looks_like_ac_shortcut(&opt_text) && !cites_repo_path(&hint) {
                        requires_input = true;
                    }
                } else if rec_seen {
                    opt.recommended = false;
                } else {
                    rec_seen = true;
                }
            }
        }

        // For strict human-input items, never keep a pre-selected option.
        if requires_input {
            for opt in &mut options {
                opt.recommended = false;
            }
        }

        out.push(AdviceItem {
            question,
            kind: final_kind.as_str().into(),
            requires_human_input: requires_input,
            hint,
            options,
        });
    }
    out
}

fn default_hint_for_kind(kind: DecisionKind) -> &'static str {
    match kind {
        DecisionKind::NeedsAccess => {
            "Cần bạn cấp quyền truy cập / cấu hình secret trước khi gỡ chặn."
        }
        DecisionKind::NeedsDevice => {
            "Cần đo/kiểm tra thực tế trên thiết bị hoặc xe thật trước khi chốt."
        }
        DecisionKind::NeedsInput => {
            "Agent đang thiếu dữ liệu bắt buộc từ bạn — hãy nhập rõ thông tin cần thiết trước khi gỡ chặn."
        }
        DecisionKind::VerifyOnBase => {
            "Ưu tiên để agent xác minh trực tiếp trên nhánh gốc (base) trước khi sửa."
        }
        DecisionKind::ScopeChoice => {
            "Ưu tiên phương án đảm bảo tính đúng đắn, đủ AC và giữ sạch phạm vi PR."
        }
        DecisionKind::Unknown => {
            "Chưa đủ căn cứ tự chọn mặc định — hãy xem kỹ bối cảnh và chọn hoặc nhập quyết định."
        }
    }
}
