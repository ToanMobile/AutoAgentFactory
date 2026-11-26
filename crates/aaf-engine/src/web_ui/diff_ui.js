/* AAF Cockpit — Structured Inline Diff Commenter (Zero-CDN, Strict CSP) */
(() => {
  "use strict";

  const STYLE_ID = "aaf-diff-ui-styles";
  const MAX_COMMENTS = 20;
  const STEER_MAX_BYTES = 2000;
  const REWORK_MAX_BYTES = 4096;
  const COMMENTS_HEADER = "[Human Diff Review Comments]";
  const utf8Encoder = typeof TextEncoder !== "undefined" ? new TextEncoder() : null;

  function utf8ByteLen(s) {
    const str = String(s ?? "");
    if (utf8Encoder) return utf8Encoder.encode(str).length;
    try { return unescape(encodeURIComponent(str)).length; } catch (_) { return str.length; }
  }

  function sanitizeInlineText(s, singleLine) {
    const str = String(s ?? "").replace(/[\x00-\x1F\x7F]/g, (ch) => (singleLine || ch !== "\n" ? " " : "\n"));
    return str.trim();
  }

  function estimateFormattedBytes(commentsList, baseNoteStr) {
    const cleanBase = sanitizeInlineText(baseNoteStr, false);
    const bullets = (Array.isArray(commentsList) ? commentsList : [])
      .filter((c) => c && sanitizeInlineText(c.body, true).length > 0)
      .map((c) => {
        const lineStart = typeof c.line_start === "number" && c.line_start > 0
          ? c.line_start
          : (typeof c.line === "number" && c.line > 0 ? c.line : 1);
        const body = sanitizeInlineText(c.body, true);
        return `- ${String(c.path || "").trim()}:${lineStart}: ${body}`;
      });
    let out = cleanBase;
    if (bullets.length) {
      out = cleanBase
        ? `${cleanBase}\n\n${COMMENTS_HEADER}\n${bullets.join("\n")}`
        : `${COMMENTS_HEADER}\n${bullets.join("\n")}`;
    }
    return utf8ByteLen(out);
  }

  function isVi() {
    try {
      if (typeof window !== "undefined" && window.AafI18n && typeof window.AafI18n.getLang === "function") {
        return window.AafI18n.getLang() === "vi";
      }
      if (typeof localStorage !== "undefined") {
        return localStorage.getItem("aaf_ui_lang") === "vi";
      }
    } catch (_) {}
    return false;
  }
  const L = (en, vi) => (isVi() ? vi : en);

  function formatByteBadge(bytes, allowRework) {
    if (bytes > REWORK_MAX_BYTES) {
      return {
        cls: "aaf-badge-trunc",
        text: L(
          `⚠ ${bytes} / ${REWORK_MAX_BYTES} B (Exceeds limit)`,
          `⚠ ${bytes} / ${REWORK_MAX_BYTES} B (Vượt giới hạn)`
        ),
      };
    }
    if (bytes > STEER_MAX_BYTES) {
      return {
        cls: "aaf-badge-lock",
        text: allowRework
          ? L(
              `⚠ ${bytes} / ${STEER_MAX_BYTES} B (> Steer · Rework ≤ ${REWORK_MAX_BYTES} B)`,
              `⚠ ${bytes} / ${STEER_MAX_BYTES} B (> Điều hướng · Làm lại ≤ ${REWORK_MAX_BYTES} B)`
            )
          : L(
              `⚠ ${bytes} / ${STEER_MAX_BYTES} B (Exceeds Steer limit)`,
              `⚠ ${bytes} / ${STEER_MAX_BYTES} B (Vượt giới hạn Điều hướng)`
            ),
      };
    }
    return {
      cls: "aaf-badge-st-U",
      text: L(`${bytes} / ${STEER_MAX_BYTES} B (Steer)`, `${bytes} / ${STEER_MAX_BYTES} B (Điều hướng)`),
    };
  }

  function esc(s) {
    return String(s ?? "").replace(/[&<>"']/g, (c) => ({
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      '"': "&quot;",
      "'": "&#39;",
    }[c]));
  }

  function injectStyles() {
    if (typeof document === "undefined" || !document.head) return;
    if (document.getElementById(STYLE_ID)) return;
    const style = document.createElement("style");
    style.id = STYLE_ID;
    style.textContent = `
.aaf-diff-root{display:flex;flex-direction:column;gap:10px;min-width:0;position:relative}
.aaf-diff-root .hidden{display:none!important}
.aaf-diff-summary{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap;padding:9px 12px;border-radius:10px;border:1px solid var(--border);background:var(--surface2)}
.aaf-diff-stats{display:flex;align-items:center;gap:7px;flex-wrap:wrap;font-size:12.5px;font-variant-numeric:tabular-nums}
.aaf-diff-controls{display:flex;align-items:center;gap:6px;flex-wrap:wrap}
.aaf-diff-controls button{min-height:36px;padding:4px 10px;font-size:12px}
.aaf-stat-add{color:var(--ok);font-weight:700;font-family:var(--mono)}
.aaf-stat-del{color:var(--err);font-weight:700;font-family:var(--mono)}
.aaf-badge{display:inline-flex;align-items:center;gap:4px;padding:2px 8px;border-radius:999px;font-size:11px;font-weight:700;white-space:nowrap;border:1px solid transparent;font-variant-numeric:tabular-nums}
.aaf-badge-lock{color:var(--warn);background:var(--warn-bg);border-color:color-mix(in srgb,var(--warn) 38%,transparent)}
.aaf-badge-trunc{color:var(--err);background:var(--err-bg);border-color:color-mix(in srgb,var(--err) 30%,transparent)}
.aaf-badge-cmt{color:var(--accent);background:var(--accent-soft,var(--info-bg));border-color:var(--accent-b,var(--border))}
.aaf-badge-st-A{color:var(--ok);background:var(--ok-bg)}
.aaf-badge-st-M{color:var(--info,var(--accent));background:var(--info-bg,var(--surface2))}
.aaf-badge-st-D{color:var(--err);background:var(--err-bg)}
.aaf-badge-st-R{color:var(--rev,var(--accent));background:var(--rev-bg,var(--surface2))}
.aaf-badge-st-U{color:var(--slate,var(--muted));background:var(--slate-bg,var(--surface2))}

.aaf-jump-bar{display:flex;gap:6px;flex-wrap:wrap;align-items:center}
.aaf-jump-pill{display:inline-flex;align-items:center;gap:5px;min-height:36px;padding:4px 10px;border-radius:999px;border:1px solid var(--border);background:var(--surface);color:var(--ink);font-size:12px;max-width:100%;cursor:pointer;transition:border-color .12s,background .12s}
.aaf-jump-pill:hover{background:var(--surface2);border-color:var(--border-strong,var(--border))}
.aaf-jump-pill.locked{border-color:color-mix(in srgb,var(--warn) 45%,var(--border))}
.aaf-jump-name{font-family:var(--mono);font-size:11.5px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:210px}
.aaf-jump-delta{font-family:var(--mono);font-size:11px;display:inline-flex;gap:4px}

.aaf-file-card{border:1px solid var(--border);border-radius:10px;background:var(--surface);overflow:hidden;box-shadow:var(--shadow-card,none)}
.aaf-file-card.locked{border-color:color-mix(in srgb,var(--warn) 45%,var(--border))}
.aaf-file-head{width:100%;display:flex;align-items:center;justify-content:space-between;gap:8px;padding:8px 11px;min-height:42px;background:var(--surface2);border:0;border-bottom:1px solid var(--border);border-radius:0;text-align:left;cursor:pointer;flex-wrap:wrap}
.aaf-file-head:hover{background:color-mix(in srgb,var(--surface2) 80%,var(--accent) 6%)}
.aaf-file-card.collapsed .aaf-file-head{border-bottom:0}
.aaf-file-head-l{display:flex;align-items:center;gap:7px;min-width:0;flex:1;flex-wrap:wrap}
.aaf-file-head-r{display:flex;align-items:center;gap:7px;flex:none}
.aaf-chev{font-size:12px;color:var(--muted);width:14px;text-align:center;flex:none}
.aaf-file-path{font-family:var(--mono);font-size:12px;font-weight:600;color:var(--ink);overflow-wrap:anywhere;word-break:break-word}
.aaf-file-body{background:var(--surface)}
.aaf-file-card.collapsed .aaf-file-body{display:none}
.aaf-file-note{padding:10px 12px;font-size:12.5px;color:var(--muted);display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap}

.aaf-diff-scroll{overflow-x:auto;-webkit-overflow-scrolling:touch;max-width:100%}
.aaf-diff-tbl{width:100%;border-collapse:collapse;font-family:var(--mono);font-size:12px;line-height:1.45;table-layout:auto}
.aaf-diff-tbl td{padding:0;border:0;vertical-align:top}
.aaf-hunk-row td{background:var(--info-bg,var(--surface2));color:var(--info,var(--accent));font-weight:600;padding:5px 10px;border-top:1px solid var(--border);border-bottom:1px solid var(--border);white-space:pre-wrap;word-break:break-word;font-size:11.5px}
.aaf-diff-tbl tr:first-child.aaf-hunk-row td{border-top:0}

.aaf-dl-add{background:color-mix(in srgb,var(--ok) 12%,var(--surface))}
.aaf-dl-del{background:color-mix(in srgb,var(--err) 12%,var(--surface))}
.aaf-dl-meta{background:var(--surface2);color:var(--muted)}
.aaf-dl-ctx{background:var(--surface)}
.aaf-dl:hover{filter:brightness(0.985)}
[data-theme="dark"] .aaf-dl:hover{filter:brightness(1.08)}

.aaf-ln{width:1%;min-width:40px;padding:2px 6px!important;text-align:right;color:var(--muted);user-select:none;border-right:1px solid color-mix(in srgb,var(--border) 65%,transparent)!important;font-variant-numeric:tabular-nums;cursor:pointer;white-space:nowrap}
.aaf-ln-btn{display:inline-flex;align-items:center;justify-content:flex-end;width:100%;min-height:24px;padding:0;border:0;background:transparent;color:inherit;font:inherit;cursor:pointer}
.aaf-ln-btn:hover{color:var(--accent);text-decoration:underline}
.aaf-ln-act{width:36px;min-width:36px;text-align:center;vertical-align:middle!important;border-right:1px solid color-mix(in srgb,var(--border) 65%,transparent)!important;padding:0 2px!important}
.aaf-add-cmt-btn{display:inline-flex;align-items:center;justify-content:center;width:28px;height:24px;min-width:28px;min-height:24px;padding:0;border-radius:6px;border:1px solid transparent;background:transparent;color:var(--accent);font-weight:800;font-size:14px;line-height:1;opacity:.22;transition:opacity .12s,background .12s,border-color .12s}
.aaf-dl:hover .aaf-add-cmt-btn,.aaf-add-cmt-btn:focus-visible,.aaf-add-cmt-btn.has-cmt{opacity:1}
.aaf-add-cmt-btn:hover{background:var(--accent);color:#fff;border-color:var(--accent)}
@media(hover:none),(max-width:768px){
  .aaf-add-cmt-btn{opacity:.85;width:36px;height:36px;min-width:36px;min-height:36px}
  .aaf-ln-btn{min-height:36px}
}
.aaf-code-cell{padding:2px 10px 2px 6px!important;white-space:pre;color:var(--ink);overflow-wrap:normal;word-break:normal}
.aaf-diff-root.aaf-wrap .aaf-code-cell{white-space:pre-wrap;word-break:break-word;overflow-wrap:anywhere}
.aaf-sign{display:inline-block;width:14px;user-select:none;font-weight:700}
.aaf-dl-add .aaf-sign{color:var(--ok)}
.aaf-dl-del .aaf-sign{color:var(--err)}
.aaf-dl-ctx .aaf-sign,.aaf-dl-meta .aaf-sign{color:var(--muted)}

.aaf-cmt-row td{background:var(--surface2);border-top:1px solid var(--border);border-bottom:1px solid var(--border);padding:8px 10px!important;font-family:var(--sans)}
.aaf-cmt-thread{display:flex;flex-direction:column;gap:7px;max-width:760px}
.aaf-cmt-card{background:var(--surface);border:1px solid var(--border);border-left:3px solid var(--accent);border-radius:8px;padding:8px 10px;display:flex;flex-direction:column;gap:6px}
.aaf-cmt-card-head{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap}
.aaf-cmt-loc{font-family:var(--mono);font-size:11.5px;font-weight:700;color:var(--accent);overflow-wrap:anywhere}
.aaf-cmt-acts{display:flex;gap:5px;flex:none}
.aaf-cmt-acts button{min-height:32px;padding:3px 8px;font-size:11.5px}
.aaf-cmt-body{font-size:13px;color:var(--ink);white-space:pre-wrap;word-break:break-word;line-height:1.45}

.aaf-composer{background:var(--surface);border:1px solid var(--accent-b,var(--accent));border-radius:9px;padding:10px;display:flex;flex-direction:column;gap:8px;box-shadow:var(--shadow-card,none)}
.aaf-composer-head{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap;font-size:12px}
.aaf-composer textarea{width:100%;min-height:68px;resize:vertical;font-family:var(--sans);font-size:13px;line-height:1.45;padding:7px 9px;border-radius:8px;border:1px solid var(--border);background:var(--bg);color:var(--ink)}
.aaf-composer-foot{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap}
.aaf-composer-btns{display:flex;gap:6px;flex-wrap:wrap}
.aaf-composer-btns button{min-height:36px;padding:5px 12px;font-size:12.5px}
.aaf-composer-err{color:var(--err);font-size:12px;font-weight:600}

.aaf-sticky-bar{position:sticky;bottom:0;z-index:10;background:color-mix(in srgb,var(--surface) 96%,transparent);backdrop-filter:blur(10px);-webkit-backdrop-filter:blur(10px);border:1px solid var(--border);border-radius:11px;padding:10px 12px;box-shadow:0 -4px 16px rgba(15,23,42,.10);display:flex;flex-direction:column;gap:8px}
.aaf-sticky-top{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap}
.aaf-sticky-meta{display:flex;align-items:center;gap:7px;flex-wrap:wrap;font-size:12.5px}
.aaf-sticky-row{display:flex;gap:8px;flex-wrap:wrap;align-items:stretch}
.aaf-sticky-note{flex:1;min-width:190px;min-height:38px;padding:7px 10px;border-radius:8px;border:1px solid var(--border);background:var(--bg);color:var(--ink);font-size:13px}
.aaf-sticky-actions{display:flex;gap:6px;flex-wrap:wrap;align-items:center}
.aaf-sticky-actions button{min-height:38px;padding:6px 12px;font-size:12.5px;font-weight:600}
.aaf-sticky-msg{font-size:12px;font-weight:600}
.aaf-sticky-msg.ok{color:var(--ok)}
.aaf-sticky-msg.err{color:var(--err)}
.aaf-sticky-preview{border-top:1px solid var(--border);padding-top:8px;display:flex;flex-direction:column;gap:5px;max-height:170px;overflow-y:auto}
.aaf-sticky-pitem{display:flex;align-items:flex-start;justify-content:space-between;gap:8px;padding:5px 8px;border-radius:7px;background:var(--surface2);font-size:12px}
.aaf-sticky-pitem-txt{min-width:0;flex:1;overflow-wrap:anywhere}
.aaf-plain-diff{background:#0B0F17;color:#E2E8F0;border:1px solid var(--border);border-radius:10px;padding:10px;max-height:520px;overflow:auto;font-family:var(--mono);font-size:11.5px;line-height:1.5;white-space:pre}
.aaf-diff-root.aaf-wrap .aaf-plain-diff{white-space:pre-wrap;word-break:break-word}
.aaf-plain-add{color:#4ADE80;background:rgba(34,197,94,.12);display:block;padding:0 4px}
.aaf-plain-del{color:#F87171;background:rgba(239,68,68,.12);display:block;padding:0 4px}
.aaf-plain-hunk{color:#60A5FA;display:block;padding:2px 4px;font-weight:600}
.aaf-plain-meta{color:#94A3B8;display:block;padding:0 4px}
.aaf-plain-ctx{display:block;padding:0 4px}
.aaf-diff-empty{padding:22px 14px;text-align:center;color:var(--muted);border:1px dashed var(--border);border-radius:10px;font-size:13px}
`;
    document.head.appendChild(style);
  }

  function normalizeStatus(raw) {
    const s = String(raw || "").trim().toLowerCase();
    if (s === "a" || s === "added" || s === "add" || s === "new") return { code: "A", label: L("Added", "Thêm mới") };
    if (s === "d" || s === "deleted" || s === "del" || s === "removed") return { code: "D", label: L("Deleted", "Đã xoá") };
    if (s === "r" || s === "renamed" || s === "rename") return { code: "R", label: L("Renamed", "Đổi tên") };
    if (s === "m" || s === "modified" || s === "mod" || s === "changed") return { code: "M", label: L("Modified", "Đã sửa") };
    return { code: "U", label: s ? s.toUpperCase() : L("Changed", "Thay đổi") };
  }

  function normalizeLineKind(raw) {
    const k = String(raw || "").trim().toLowerCase();
    if (k === "add" || k === "+" || k === "addition" || k === "insert") return "add";
    if (k === "del" || k === "-" || k === "deletion" || k === "remove") return "del";
    if (k === "meta" || k === "no_newline" || k === "info") return "meta";
    return "ctx";
  }

  function basenameOf(path) {
    const p = String(path || "");
    const idx = p.lastIndexOf("/");
    return idx >= 0 ? p.slice(idx + 1) : p;
  }

  function lineAnchorKey(fileIdx, hunkIdx, lineIdx) {
    return `${fileIdx}:${hunkIdx}:${lineIdx}`;
  }

  function formatCommentSummary(comments, baseNote) {
    const parts = [];
    const trimmed = String(baseNote || "").trim();
    if (trimmed) parts.push(trimmed);
    if (Array.isArray(comments) && comments.length) {
      parts.push(
        comments
          .map((c) => {
            const loc = c.line > 0
              ? `${c.path}:L${c.line}${c.side === "old" ? " (old)" : ""}`
              : `${c.path}`;
            return `- [${loc}] ${c.body}`;
          })
          .join("\n")
      );
    }
    return parts.join("\n\n");
  }

  function renderPlainDiffHtml(raw) {
    if (!raw || !String(raw).trim()) {
      return `<div class="aaf-diff-empty">${esc(L("No code diff available in worktree.", "Không có thay đổi mã nguồn trong worktree."))}</div>`;
    }
    return `<div class="aaf-plain-diff">` + String(raw).split("\n").map((ln) => {
      const safe = esc(ln) || " ";
      if (ln.startsWith("+++") || ln.startsWith("---") || ln.startsWith("diff --git") || ln.startsWith("index ")) {
        return `<span class="aaf-plain-meta">${safe}</span>`;
      }
      if (ln.startsWith("@@")) return `<span class="aaf-plain-hunk">${safe}</span>`;
      if (ln.startsWith("+")) return `<span class="aaf-plain-add">${safe}</span>`;
      if (ln.startsWith("-")) return `<span class="aaf-plain-del">${safe}</span>`;
      return `<span class="aaf-plain-ctx">${safe}</span>`;
    }).join("") + `</div>`;
  }

  function copyFallback(text) {
    if (typeof navigator !== "undefined" && navigator.clipboard && typeof navigator.clipboard.writeText === "function") {
      return navigator.clipboard.writeText(text).then(() => true).catch(() => false);
    }
    if (typeof document === "undefined") return Promise.resolve(false);
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.cssText = "position:fixed;top:-9999px;left:-9999px;opacity:0";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      ta.remove();
      return Promise.resolve(Boolean(ok));
    } catch (_) {
      return Promise.resolve(false);
    }
  }

  function renderStructuredDiff(containerEl, diffResp, taskState, onSubmitComments, opts) {
    if (!containerEl) return null;
    injectStyles();
    opts = opts || {};

    const resp = diffResp || {};
    const stateStr = String(taskState || "").trim().toLowerCase();
    const isTerminal = stateStr === "done" || stateStr === "cancelled";
    const isReadOnly = Boolean(opts.readOnly);
    const canSubmit = !isTerminal && !isReadOnly && typeof onSubmitComments === "function";

    if (resp.available === false) {
      containerEl.innerHTML = `<div class="aaf-diff-empty">${esc(resp.note || L("No worktree diff available.", "Không có thay đổi nào trong worktree."))}</div>`;
      return null;
    }

    const files = Array.isArray(resp.files) ? resp.files : [];
    let wrapLines = Boolean(opts.wrap);

    // Graceful fallback to plain unified diff when structured `files` is missing/empty
    if (!files.length) {
      const root = document.createElement("div");
      root.className = "aaf-diff-root" + (wrapLines ? " aaf-wrap" : "");
      root.innerHTML = `
        <div class="aaf-diff-summary">
          <div class="aaf-diff-stats">
            <span class="mono muted">${esc(resp.branch || resp.worktree_path || L("Plain diff", "Bản thay đổi thô"))}</span>
            ${resp.truncated ? `<span class="aaf-badge aaf-badge-trunc">${esc(L("Truncated", "Đã cắt ngắn"))}</span>` : ""}
          </div>
          <div class="aaf-diff-controls">
            <button type="button" data-plain-wrap>${esc(wrapLines ? L("Disable wrap", "Tắt xuống dòng") : L("Wrap lines", "Tự xuống dòng"))}</button>
          </div>
        </div>
        ${resp.stat ? `<pre class="mono muted" style="margin:0;white-space:pre-wrap;background:var(--surface2);padding:8px 10px;border-radius:8px;border:1px solid var(--border)">${esc(resp.stat)}</pre>` : ""}
        ${renderPlainDiffHtml(resp.diff)}
        ${resp.truncated ? `<div class="muted" style="font-size:12px">${esc(L("Diff output was truncated by server size limit.", "Nội dung thay đổi đã bị cắt ngắn do vượt giới hạn kích thước."))}</div>` : ""}
      `;
      containerEl.innerHTML = "";
      containerEl.appendChild(root);
      const wrapBtn = root.querySelector("[data-plain-wrap]");
      if (wrapBtn) {
        wrapBtn.addEventListener("click", () => {
          wrapLines = !wrapLines;
          root.classList.toggle("aaf-wrap", wrapLines);
          wrapBtn.textContent = wrapLines ? L("Disable wrap", "Tắt xuống dòng") : L("Wrap lines", "Tự xuống dòng");
        });
      }
      return { getComments: () => [] };
    }

    // Compute summary metrics defensively
    const sum = resp.summary || {};
    const totalFiles = Number.isFinite(Number(sum.files_changed)) ? Number(sum.files_changed) : files.length;
    const totalAdd = Number.isFinite(Number(sum.additions ?? sum.insertions))
      ? Number(sum.additions ?? sum.insertions)
      : files.reduce((acc, f) => acc + (Number(f.additions) || 0), 0);
    const totalDel = Number.isFinite(Number(sum.deletions))
      ? Number(sum.deletions)
      : files.reduce((acc, f) => acc + (Number(f.deletions) || 0), 0);
    const lockedCount = Number.isFinite(Number(sum.head_locked_count))
      ? Number(sum.head_locked_count)
      : files.filter((f) => Boolean(f.head_locked)).length;

    // Local interactive state
    const collapsed = new Set();
    const comments = [];
    let nextCommentId = 1;
    let activeComposer = null; // { anchorKey, fileIdx, path, line, oldLine, newLine, side, codeLine, editId, draftText, error }
    let baseNote = "";
    let showPreview = false;
    let submitting = false;
    let statusMsg = null; // { kind: "ok" | "err", text: string }

    const root = document.createElement("div");
    root.className = "aaf-diff-root" + (wrapLines ? " aaf-wrap" : "");
    containerEl.innerHTML = "";
    containerEl.appendChild(root);

    function commentsForFile(path) {
      return comments.filter((c) => c.path === path);
    }

    function commentsForAnchor(anchorKey) {
      return comments.filter((c) => c.anchorKey === anchorKey);
    }

    function exportComments() {
      return comments.map((c) => ({
        id: c.id,
        path: c.path,
        line: c.line,
        line_start: typeof c.line === "number" && c.line > 0 ? c.line : 0,
        old_line: c.oldLine,
        new_line: c.newLine,
        side: c.side,
        code_line: c.codeLine,
        body: c.body,
      }));
    }

    function previewCommentsWithComposer(comp) {
      const draftBody = String((comp && comp.draftText) || "").trim();
      const list = comments.map((c) => (
        comp && comp.editId != null && c.id === comp.editId
          ? Object.assign({}, c, { body: draftBody })
          : c
      ));
      if (comp && comp.editId == null && draftBody) {
        list.push({ path: comp.path, line: comp.line, body: draftBody });
      }
      return list;
    }

    function renderComposerHtml(comp) {
      const loc = comp.line > 0
        ? `${comp.path}:L${comp.line}${comp.side === "old" ? L(" (old)", " (cũ)") : ""}`
        : `${comp.path} ${L("(file note)", "(ghi chú tệp)")}`;
      const isEdit = comp.editId != null;
      const draftBytes = estimateFormattedBytes(previewCommentsWithComposer(comp), baseNote);
      const byteBadge = formatByteBadge(draftBytes, stateStr === "review");
      return `
        <div class="aaf-composer" data-composer>
          <div class="aaf-composer-head">
            <span class="aaf-cmt-loc">${esc(isEdit ? L("Edit comment · ", "Sửa góp ý · ") : L("Comment on ", "Góp ý tại "))}${esc(loc)}</span>
            <span style="display:inline-flex;align-items:center;gap:6px;flex-wrap:wrap">
              <span class="aaf-badge ${byteBadge.cls}" data-composer-bytes>${esc(byteBadge.text)}</span>
              <span class="muted">${comments.length}/${MAX_COMMENTS}</span>
            </span>
          </div>
          <textarea data-composer-input placeholder="${esc(L("Add inline review comment… (Cmd/Ctrl+Enter to save)", "Nhập góp ý duyệt mã… (Cmd/Ctrl+Enter để lưu)"))}">${esc(comp.draftText || "")}</textarea>
          ${comp.error ? `<div class="aaf-composer-err">${esc(comp.error)}</div>` : ""}
          <div class="aaf-composer-foot">
            <span class="muted" style="font-size:11.5px">${esc(L("Queued locally until you submit below", "Lưu tạm cho đến khi bạn bấm gửi bên dưới"))}</span>
            <div class="aaf-composer-btns">
              <button type="button" data-composer-cancel>${esc(L("Cancel", "Huỷ"))}</button>
              <button type="button" class="btn-primary" data-composer-save>${esc(isEdit ? L("Update Comment", "Cập nhật góp ý") : L("Save Comment", "Lưu góp ý"))}</button>
            </div>
          </div>
        </div>
      `;
    }

    function renderCommentCardHtml(c) {
      const loc = c.line > 0
        ? `${c.path}:L${c.line}${c.side === "old" ? L(" (old)", " (cũ)") : ""}`
        : `${c.path} ${L("(file note)", "(ghi chú tệp)")}`;
      return `
        <div class="aaf-cmt-card" data-cmt-id="${c.id}">
          <div class="aaf-cmt-card-head">
            <span class="aaf-cmt-loc">${esc(loc)}</span>
            <div class="aaf-cmt-acts">
              <button type="button" data-cmt-edit="${c.id}">${esc(L("Edit", "Sửa"))}</button>
              <button type="button" data-cmt-del="${c.id}" aria-label="${esc(L("Remove comment", "Xoá góp ý"))}">${esc(L("Remove", "Xoá"))}</button>
            </div>
          </div>
          <div class="aaf-cmt-body">${esc(c.body)}</div>
        </div>
      `;
    }

    function renderFileCardHtml(file, fileIdx) {
      const path = String(file.path || "(unknown)");
      const st = normalizeStatus(file.status);
      const isCollapsed = collapsed.has(fileIdx);
      const fileCmts = commentsForFile(path);
      const adds = Number(file.additions) || 0;
      const dels = Number(file.deletions) || 0;
      const hunks = Array.isArray(file.hunks) ? file.hunks : [];

      let bodyHtml = "";
      if (file.binary) {
        const fileAnchor = lineAnchorKey(fileIdx, -1, -1);
        const existing = commentsForAnchor(fileAnchor);
        const showComp = activeComposer && activeComposer.anchorKey === fileAnchor;
        bodyHtml = `
          <div class="aaf-file-note">
            <span>${esc(L("Binary file changed — line-level diff unavailable.", "Tệp nhị phân thay đổi — không hiển thị so sánh từng dòng."))}</span>
            <button type="button" class="btn-sm" data-file-note="${fileIdx}">${esc(L("+ Add file comment", "+ Thêm góp ý cho tệp"))}</button>
          </div>
          ${(existing.length || showComp) ? `
            <div style="padding:8px 10px;background:var(--surface2);border-top:1px solid var(--border)">
              <div class="aaf-cmt-thread">
                ${existing.map((c) => renderCommentCardHtml(c)).join("")}
                ${showComp ? renderComposerHtml(activeComposer) : ""}
              </div>
            </div>` : ""}
        `;
      } else if (!hunks.length) {
        const fileAnchor = lineAnchorKey(fileIdx, -1, -1);
        const existing = commentsForAnchor(fileAnchor);
        const showComp = activeComposer && activeComposer.anchorKey === fileAnchor;
        bodyHtml = `
          <div class="aaf-file-note">
            <span>${esc(L("No text hunks (file mode change, rename, or empty file).", "Không có đoạn thay đổi văn bản (đổi quyền, đổi tên hoặc tệp trống)."))}</span>
            <button type="button" class="btn-sm" data-file-note="${fileIdx}">${esc(L("+ Add file comment", "+ Thêm góp ý cho tệp"))}</button>
          </div>
          ${(existing.length || showComp) ? `
            <div style="padding:8px 10px;background:var(--surface2);border-top:1px solid var(--border)">
              <div class="aaf-cmt-thread">
                ${existing.map((c) => renderCommentCardHtml(c)).join("")}
                ${showComp ? renderComposerHtml(activeComposer) : ""}
              </div>
            </div>` : ""}
        `;
      } else {
        const rows = [];
        hunks.forEach((hunk, hunkIdx) => {
          const header = String(hunk.header || `@@ -${hunk.old_start ?? 0} +${hunk.new_start ?? 0} @@`);
          rows.push(`<tr class="aaf-hunk-row"><td colspan="4">${esc(header)}</td></tr>`);
          const lines = Array.isArray(hunk.lines) ? hunk.lines : [];
          lines.forEach((ln, lineIdx) => {
            const kind = normalizeLineKind(ln.kind);
            const oldLn = ln.old_ln != null && ln.old_ln !== "" ? Number(ln.old_ln) : null;
            const newLn = ln.new_ln != null && ln.new_ln !== "" ? Number(ln.new_ln) : null;
            const text = String(ln.text ?? ln.content ?? ln.line ?? "");
            const sign = kind === "add" ? "+" : kind === "del" ? "-" : kind === "meta" ? "" : " ";
            const aKey = lineAnchorKey(fileIdx, hunkIdx, lineIdx);
            const lineCmts = commentsForAnchor(aKey);
            const hasComp = activeComposer && activeComposer.anchorKey === aKey;
            const canCommentLine = kind !== "meta" && (newLn != null || oldLn != null);
            const btnAttrs = canCommentLine
              ? `data-open-cmt="${esc(aKey)}" data-fi="${fileIdx}" data-hi="${hunkIdx}" data-li="${lineIdx}"`
              : "";

            rows.push(`
              <tr class="aaf-dl aaf-dl-${kind}">
                <td class="aaf-ln">${oldLn != null ? `<button type="button" class="aaf-ln-btn" ${btnAttrs} title="${esc(L("Comment on ", "Góp ý tại "))}${esc(path)}:L${oldLn}">${oldLn}</button>` : ""}</td>
                <td class="aaf-ln">${newLn != null ? `<button type="button" class="aaf-ln-btn" ${btnAttrs} title="${esc(L("Comment on ", "Góp ý tại "))}${esc(path)}:L${newLn}">${newLn}</button>` : ""}</td>
                <td class="aaf-ln-act">${canCommentLine ? `<button type="button" class="aaf-add-cmt-btn ${lineCmts.length ? "has-cmt" : ""}" ${btnAttrs} aria-label="${esc(L(`Add comment at ${path} line ${newLn ?? oldLn}`, `Thêm góp ý tại ${path} dòng ${newLn ?? oldLn}`))}" title="${esc(L("Add inline comment", "Thêm góp ý tại dòng này"))}">+</button>` : ""}</td>
                <td class="aaf-code-cell"><span class="aaf-sign">${esc(sign)}</span>${esc(text)}</td>
              </tr>
            `);

            if (lineCmts.length || hasComp) {
              rows.push(`
                <tr class="aaf-cmt-row">
                  <td colspan="4">
                    <div class="aaf-cmt-thread">
                      ${lineCmts.map((c) => renderCommentCardHtml(c)).join("")}
                      ${hasComp ? renderComposerHtml(activeComposer) : ""}
                    </div>
                  </td>
                </tr>
              `);
            }
          });
        });
        bodyHtml = `<div class="aaf-diff-scroll"><table class="aaf-diff-tbl"><tbody>${rows.join("")}</tbody></table></div>`;
      }

      return `
        <section class="aaf-file-card ${file.head_locked ? "locked" : ""} ${isCollapsed ? "collapsed" : ""}" data-file-card="${fileIdx}" id="aaf-diff-file-${fileIdx}">
          <button type="button" class="aaf-file-head" data-toggle-file="${fileIdx}" aria-expanded="${!isCollapsed}">
            <span class="aaf-file-head-l">
              <span class="aaf-chev" aria-hidden="true">${isCollapsed ? "▸" : "▾"}</span>
              <span class="aaf-badge aaf-badge-st-${st.code}" title="${esc(st.label)}">${esc(st.code)}</span>
              <span class="aaf-file-path">${esc(path)}</span>
              ${file.head_locked ? `<span class="aaf-badge aaf-badge-lock">🔒 ${esc(L("HEAD-LOCKED", "KHOÁ BẢO VỆ"))}</span>` : ""}
              ${fileCmts.length ? `<span class="aaf-badge aaf-badge-cmt">${esc(L(`${fileCmts.length} comment${fileCmts.length > 1 ? "s" : ""}`, `${fileCmts.length} góp ý`))}</span>` : ""}
            </span>
            <span class="aaf-file-head-r">
              <span class="aaf-stat-add">+${adds}</span>
              <span class="aaf-stat-del">-${dels}</span>
            </span>
          </button>
          <div class="aaf-file-body">${bodyHtml}</div>
        </section>
      `;
    }

    function renderStickyBarHtml() {
      const count = comments.length;
      const hasContent = count > 0 || baseNote.trim().length > 0;
      const cmtSuffix = count > 0 ? L(` (${count} comment${count > 1 ? "s" : ""})`, ` (${count} góp ý)`) : "";
      const payloadBytes = hasContent ? estimateFormattedBytes(comments, baseNote) : 0;
      const byteBadge = formatByteBadge(payloadBytes, stateStr === "review");
      const overSteerLimit = payloadBytes > STEER_MAX_BYTES;
      const stLabel = stateStr === "done" ? L("done", "đã hoàn thành") : (stateStr === "cancelled" ? L("cancelled", "đã huỷ") : stateStr);
      let actionButtons = "";

      if (isTerminal) {
        actionButtons = `
          <span class="muted" style="font-size:12px">${esc(L(`Task is ${stLabel} — read-only`, `Việc ${stLabel} — chỉ đọc`))}</span>
          <button type="button" data-copy-cmts ${!hasContent ? "disabled" : ""}>${esc(L("Copy Comments", "Sao chép góp ý"))}</button>
        `;
      } else if (isReadOnly) {
        actionButtons = `
          <span class="muted" style="font-size:12px">${esc(L("Read-only mode", "Chế độ chỉ đọc"))}</span>
          <button type="button" data-copy-cmts ${!hasContent ? "disabled" : ""}>${esc(L("Copy Comments", "Sao chép góp ý"))}</button>
        `;
      } else if (stateStr === "review") {
        actionButtons = `
          <button type="button" data-submit-mode="steer" ${submitting || !hasContent ? "disabled" : ""} ${overSteerLimit ? `title="${esc(L(`Exceeds ${STEER_MAX_BYTES}-byte Steer limit`, `Vượt giới hạn ${STEER_MAX_BYTES} byte của Điều hướng`))}"` : ""}>
            ${esc(submitting ? L("Sending…", "Đang gửi…") : L(`Send as Steer${count > 0 ? ` (${count})` : ""}`, `Gửi điều hướng${count > 0 ? ` (${count})` : ""}`))}
          </button>
          <button type="button" class="btn-danger" data-submit-mode="rework" ${submitting || !hasContent ? "disabled" : ""}>
            ${esc(submitting ? L("Sending…", "Đang gửi…") : `${L("Request Rework", "Yêu cầu làm lại")}${cmtSuffix}`)}
          </button>
        `;
      } else {
        actionButtons = `
          <button type="button" class="btn-primary" data-submit-mode="steer" ${submitting || !hasContent ? "disabled" : ""} ${overSteerLimit ? `title="${esc(L(`Exceeds ${STEER_MAX_BYTES}-byte Steer limit`, `Vượt giới hạn ${STEER_MAX_BYTES} byte của Điều hướng`))}"` : ""}>
            ${esc(submitting ? L("Sending…", "Đang gửi…") : `${L("Send as Steer", "Gửi điều hướng")}${cmtSuffix}`)}
          </button>
        `;
      }

      const previewHtml = showPreview && count > 0
        ? `<div class="aaf-sticky-preview">${comments.map((c) => {
            const loc = c.line > 0 ? `${c.path}:L${c.line}` : c.path;
            return `<div class="aaf-sticky-pitem">
              <div class="aaf-sticky-pitem-txt"><span class="aaf-cmt-loc">${esc(loc)}</span> — ${esc(c.body)}</div>
              <button type="button" class="btn-sm" data-cmt-del="${c.id}" aria-label="${esc(L("Remove comment", "Xoá góp ý"))}">×</button>
            </div>`;
          }).join("")}</div>`
        : "";

      return `
        <div class="aaf-sticky-bar" data-sticky-bar>
          <div class="aaf-sticky-top">
            <div class="aaf-sticky-meta">
              <span class="aaf-badge ${count ? "aaf-badge-cmt" : "aaf-badge-st-U"}">${esc(L(`${count}/${MAX_COMMENTS} inline comment${count === 1 ? "" : "s"}`, `${count}/${MAX_COMMENTS} góp ý theo dòng`))}</span>
              <span class="aaf-badge ${byteBadge.cls} ${hasContent ? "" : "hidden"}" data-byte-counter>${esc(byteBadge.text)}</span>
              ${count ? `<button type="button" class="btn-sm" data-toggle-preview>${esc(showPreview ? L("Hide list", "Ẩn danh sách") : L("Review list", "Xem danh sách"))}</button><button type="button" class="btn-sm" data-clear-all>${esc(L("Clear all", "Xoá hết"))}</button>` : `<span class="muted">${esc(L("Click any line number or + to attach inline review notes", "Bấm số dòng hoặc dấu + để thêm góp ý duyệt mã"))}</span>`}
            </div>
            ${statusMsg ? `<span class="aaf-sticky-msg ${esc(statusMsg.kind)}">${esc(statusMsg.text)}</span>` : ""}
          </div>
          ${previewHtml}
          <div class="aaf-sticky-row">
            <input type="text" class="aaf-sticky-note" data-base-note placeholder="${esc(L("Optional summary note for the agent…", "Ghi chú tóm tắt cho agent (tuỳ chọn)…"))}" value="${esc(baseNote)}" ${submitting || isTerminal || isReadOnly ? "disabled" : ""}>
            <div class="aaf-sticky-actions">${actionButtons}</div>
          </div>
        </div>
      `;
    }

    function render(focusComposer) {
      const allCollapsed = collapsed.size >= files.length && files.length > 0;
      const jumpPillsHtml = files.map((f, idx) => {
        const path = String(f.path || "(unknown)");
        const st = normalizeStatus(f.status);
        const fCmts = commentsForFile(path).length;
        return `
          <button type="button" class="aaf-jump-pill ${f.head_locked ? "locked" : ""}" data-jump-file="${idx}" title="${esc(path)}">
            <span class="aaf-badge aaf-badge-st-${st.code}">${esc(st.code)}</span>
            <span class="aaf-jump-name">${esc(basenameOf(path))}</span>
            <span class="aaf-jump-delta"><span class="aaf-stat-add">+${Number(f.additions) || 0}</span><span class="aaf-stat-del">-${Number(f.deletions) || 0}</span></span>
            ${f.head_locked ? `<span class="aaf-badge aaf-badge-lock">🔒</span>` : ""}
            ${fCmts ? `<span class="aaf-badge aaf-badge-cmt">${fCmts}</span>` : ""}
          </button>
        `;
      }).join("");

      root.classList.toggle("aaf-wrap", wrapLines);
      root.innerHTML = `
        <div class="aaf-diff-summary">
          <div class="aaf-diff-stats">
            <strong>${esc(L(`${totalFiles} file${totalFiles === 1 ? "" : "s"} changed`, `${totalFiles} tệp thay đổi`))}</strong>
            <span class="aaf-stat-add">+${totalAdd}</span>
            <span class="aaf-stat-del">-${totalDel}</span>
            ${lockedCount > 0 ? `<span class="aaf-badge aaf-badge-lock">🔒 ${esc(L(`${lockedCount} HEAD-LOCKED`, `${lockedCount} KHOÁ BẢO VỆ`))}</span>` : ""}
            ${resp.truncated ? `<span class="aaf-badge aaf-badge-trunc">${esc(L("Truncated", "Đã cắt ngắn"))}</span>` : ""}
          </div>
          <div class="aaf-diff-controls">
            <button type="button" data-toggle-wrap>${esc(wrapLines ? L("Disable wrap", "Tắt xuống dòng") : L("Wrap lines", "Tự xuống dòng"))}</button>
            <button type="button" data-toggle-all>${esc(allCollapsed ? L("Expand all", "Mở tất cả") : L("Collapse all", "Thu gọn hết"))}</button>
          </div>
        </div>
        ${files.length > 1 || lockedCount > 0 ? `<div class="aaf-jump-bar" role="navigation" aria-label="${esc(L("Changed files", "Các tệp thay đổi"))}">${jumpPillsHtml}</div>` : ""}
        <div class="aaf-file-list" style="display:flex;flex-direction:column;gap:10px">
          ${files.map((f, idx) => renderFileCardHtml(f, idx)).join("")}
        </div>
        ${renderStickyBarHtml()}
      `;

      if (focusComposer) {
        const ta = root.querySelector("[data-composer-input]");
        if (ta) {
          ta.focus();
          const len = ta.value.length;
          try { ta.setSelectionRange(len, len); } catch (_) {}
        }
      }
    }

    function openLineComposer(fileIdx, hunkIdx, lineIdx) {
      const file = files[fileIdx];
      if (!file) return;
      const path = String(file.path || "(unknown)");
      const aKey = lineAnchorKey(fileIdx, hunkIdx, lineIdx);
      let oldLine = null;
      let newLine = null;
      let codeLine = "";
      let side = "new";
      let line = 0;

      if (hunkIdx >= 0 && lineIdx >= 0) {
        const hunk = (file.hunks || [])[hunkIdx];
        const ln = hunk && (hunk.lines || [])[lineIdx];
        if (!ln) return;
        oldLine = ln.old_ln != null && ln.old_ln !== "" ? Number(ln.old_ln) : null;
        newLine = ln.new_ln != null && ln.new_ln !== "" ? Number(ln.new_ln) : null;
        codeLine = String(ln.text ?? ln.content ?? ln.line ?? "");
        if (newLine != null) {
          line = newLine;
          side = "new";
        } else if (oldLine != null) {
          line = oldLine;
          side = "old";
        }
      }

      const limitErr = comments.length >= MAX_COMMENTS
        ? L(
            `Maximum ${MAX_COMMENTS} inline comments reached. Edit or remove an existing comment first.`,
            `Đã đạt tối đa ${MAX_COMMENTS} góp ý theo dòng. Hãy sửa hoặc xoá bớt một góp ý trước.`
          )
        : "";
      statusMsg = null;
      activeComposer = {
        anchorKey: aKey,
        fileIdx,
        path,
        line,
        oldLine,
        newLine,
        side,
        codeLine,
        editId: null,
        draftText: "",
        error: limitErr,
      };
      render(true);
    }

    function saveActiveComposer() {
      if (!activeComposer) return;
      const ta = root.querySelector("[data-composer-input]");
      const text = (ta ? ta.value : activeComposer.draftText || "").trim();
      activeComposer.draftText = ta ? ta.value : activeComposer.draftText;
      if (!text) {
        activeComposer.error = L("Please enter a comment before saving.", "Vui lòng nhập góp ý trước khi lưu.");
        render(true);
        return;
      }
      if (activeComposer.editId != null) {
        const existing = comments.find((c) => c.id === activeComposer.editId);
        if (existing) existing.body = text;
        activeComposer = null;
        statusMsg = null;
        render(false);
        return;
      }
      if (comments.length >= MAX_COMMENTS) {
        activeComposer.error = L(
          `Maximum ${MAX_COMMENTS} inline comments reached. Remove one before adding more.`,
          `Đã đạt tối đa ${MAX_COMMENTS} góp ý theo dòng. Hãy xoá bớt trước khi thêm mới.`
        );
        render(true);
        return;
      }
      comments.push({
        id: nextCommentId++,
        anchorKey: activeComposer.anchorKey,
        path: activeComposer.path,
        line: activeComposer.line,
        oldLine: activeComposer.oldLine,
        newLine: activeComposer.newLine,
        side: activeComposer.side,
        codeLine: activeComposer.codeLine,
        body: text,
      });
      activeComposer = null;
      statusMsg = null;
      render(false);
    }

    async function handleSubmit(mode) {
      if (submitting || !canSubmit) return;
      if (mode === "rework" && stateStr !== "review") return;
      const exported = exportComments();
      const note = baseNote.trim();
      if (!exported.length && !note) return;

      submitting = true;
      statusMsg = null;
      render(false);
      try {
        await onSubmitComments(mode, note, exported);
        comments.length = 0;
        baseNote = "";
        activeComposer = null;
        showPreview = false;
        statusMsg = {
          kind: "ok",
          text: mode === "rework"
            ? L("Rework request sent.", "Đã gửi yêu cầu làm lại.")
            : L("Steer feedback sent.", "Đã gửi góp ý điều hướng."),
        };
      } catch (err) {
        statusMsg = {
          kind: "err",
          text: (err && err.message) ? err.message : L("Failed to submit comments.", "Gửi góp ý thất bại."),
        };
      } finally {
        submitting = false;
        render(false);
      }
    }

    // Event delegation on root
    root.addEventListener("click", (e) => {
      const target = e.target.closest("button");
      if (!target || !root.contains(target)) return;

      if (target.hasAttribute("data-toggle-wrap")) {
        wrapLines = !wrapLines;
        render(false);
        return;
      }
      if (target.hasAttribute("data-toggle-all")) {
        if (collapsed.size >= files.length) collapsed.clear();
        else files.forEach((_, i) => collapsed.add(i));
        render(false);
        return;
      }
      if (target.hasAttribute("data-jump-file")) {
        const idx = Number(target.dataset.jumpFile);
        if (collapsed.has(idx)) {
          collapsed.delete(idx);
          render(false);
        }
        const card = root.querySelector(`[data-file-card="${idx}"]`);
        if (card && typeof card.scrollIntoView === "function") {
          card.scrollIntoView({ behavior: "smooth", block: "start" });
        }
        return;
      }
      if (target.hasAttribute("data-toggle-file")) {
        const idx = Number(target.dataset.toggleFile);
        if (collapsed.has(idx)) collapsed.delete(idx);
        else collapsed.add(idx);
        render(false);
        return;
      }
      if (target.hasAttribute("data-open-cmt")) {
        openLineComposer(
          Number(target.dataset.fi),
          Number(target.dataset.hi),
          Number(target.dataset.li)
        );
        return;
      }
      if (target.hasAttribute("data-file-note")) {
        openLineComposer(Number(target.dataset.fileNote), -1, -1);
        return;
      }
      if (target.hasAttribute("data-composer-cancel")) {
        activeComposer = null;
        render(false);
        return;
      }
      if (target.hasAttribute("data-composer-save")) {
        saveActiveComposer();
        return;
      }
      if (target.hasAttribute("data-cmt-edit")) {
        const id = Number(target.dataset.cmtEdit);
        const existing = comments.find((c) => c.id === id);
        if (!existing) return;
        activeComposer = {
          anchorKey: existing.anchorKey,
          fileIdx: -1,
          path: existing.path,
          line: existing.line,
          oldLine: existing.oldLine,
          newLine: existing.newLine,
          side: existing.side,
          codeLine: existing.codeLine,
          editId: existing.id,
          draftText: existing.body,
          error: "",
        };
        render(true);
        return;
      }
      if (target.hasAttribute("data-cmt-del")) {
        const id = Number(target.dataset.cmtDel);
        const idx = comments.findIndex((c) => c.id === id);
        if (idx >= 0) comments.splice(idx, 1);
        if (activeComposer && activeComposer.editId === id) activeComposer = null;
        if (!comments.length) showPreview = false;
        render(false);
        return;
      }
      if (target.hasAttribute("data-toggle-preview")) {
        showPreview = !showPreview;
        render(false);
        return;
      }
      if (target.hasAttribute("data-clear-all")) {
        comments.length = 0;
        showPreview = false;
        if (activeComposer && activeComposer.editId != null) activeComposer = null;
        render(false);
        return;
      }
      if (target.hasAttribute("data-copy-cmts")) {
        const text = formatCommentSummary(exportComments(), baseNote);
        if (!text) return;
        copyFallback(text).then((ok) => {
          statusMsg = {
            kind: ok ? "ok" : "err",
            text: ok ? L("Copied comments to clipboard.", "Đã sao chép góp ý.") : L("Copy failed.", "Sao chép thất bại."),
          };
          render(false);
        });
        return;
      }
      if (target.hasAttribute("data-submit-mode")) {
        handleSubmit(target.dataset.submitMode);
      }
    });

    root.addEventListener("input", (e) => {
      if (e.target.hasAttribute("data-base-note")) {
        baseNote = e.target.value;
        // Update submit button enabled state & live byte counter without re-rendering input focus away
        const hasContent = comments.length > 0 || baseNote.trim().length > 0;
        const payloadBytes = hasContent ? estimateFormattedBytes(comments, baseNote) : 0;
        const badgeEl = root.querySelector("[data-byte-counter]");
        if (badgeEl) {
          const badge = formatByteBadge(payloadBytes, stateStr === "review");
          badgeEl.className = `aaf-badge ${badge.cls}${hasContent ? "" : " hidden"}`;
          badgeEl.textContent = badge.text;
        }
        root.querySelectorAll("[data-submit-mode],[data-copy-cmts]").forEach((btn) => {
          btn.disabled = submitting || !hasContent;
        });
      } else if (e.target.hasAttribute("data-composer-input") && activeComposer) {
        activeComposer.draftText = e.target.value;
        const compBadge = root.querySelector("[data-composer-bytes]");
        if (compBadge) {
          const draftBytes = estimateFormattedBytes(previewCommentsWithComposer(activeComposer), baseNote);
          const badge = formatByteBadge(draftBytes, stateStr === "review");
          compBadge.className = `aaf-badge ${badge.cls}`;
          compBadge.textContent = badge.text;
        }
      }
    });

    root.addEventListener("keydown", (e) => {
      if (e.target.hasAttribute("data-composer-input")) {
        if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
          e.preventDefault();
          saveActiveComposer();
        } else if (e.key === "Escape") {
          e.preventDefault();
          activeComposer = null;
          render(false);
        }
      }
    });

    render(false);

    return {
      getComments: exportComments,
      clearComments: () => {
        comments.length = 0;
        baseNote = "";
        activeComposer = null;
        render(false);
      },
    };
  }

  const api = {
    MAX_COMMENTS,
    renderStructuredDiff,
    formatCommentSummary,
  };

  if (typeof window !== "undefined") {
    window.AafDiffUI = api;
  } else if (typeof globalThis !== "undefined") {
    globalThis.AafDiffUI = api;
  }
})();
