/* AAF Cockpit & Studio — Visual SVG Pipeline DAG, SOP Runs & Studio Health Score (Zero-CDN, Strict CSP) */
(() => {
  "use strict";

  const STYLE_ID = "aaf-pipeline-ui-styles";

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
/* --- Task Pipeline Fit-to-Screen Stepper Grid --- */
.aaf-pipe-wrap{display:flex;flex-direction:column;gap:9px;min-width:0;width:100%}
.aaf-pipe-banner{background:var(--surface2);border:1px solid var(--border);border-left:4px solid var(--accent);border-radius:11px;padding:10px 12px;display:flex;flex-direction:column;gap:7px;min-width:0}
.aaf-pipe-banner.st-passed{border-left-color:var(--ok);background:color-mix(in srgb,var(--ok-bg) 55%,var(--surface2))}
.aaf-pipe-banner.st-running{border-left-color:var(--info,var(--accent));background:color-mix(in srgb,var(--info-bg,var(--surface2)) 70%,var(--surface2))}
.aaf-pipe-banner.st-waiting_human{border-left-color:var(--warn);background:color-mix(in srgb,var(--warn-bg) 65%,var(--surface2))}
.aaf-pipe-banner.st-failed{border-left-color:var(--err);background:color-mix(in srgb,var(--err-bg) 65%,var(--surface2))}
.aaf-pipe-banner.st-skipped{border-left-color:var(--slate,var(--muted))}
.aaf-pipe-banner-top{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap}
.aaf-pipe-now{display:flex;align-items:center;gap:7px;min-width:0;flex-wrap:wrap}
.aaf-pipe-live-dot{width:9px;height:9px;border-radius:50%;flex:none;display:inline-block}
@keyframes aafPipeDotPulse{0%,100%{box-shadow:0 0 0 0 currentColor}55%{box-shadow:0 0 0 6px transparent}}
.aaf-pipe-live-dot.pulsing{animation:aafPipeDotPulse 1.7s ease-out infinite}
@media(prefers-reduced-motion:reduce){.aaf-pipe-live-dot.pulsing{animation:none}}
.aaf-pipe-now-title{font-weight:700;font-size:13px;color:var(--ink);letter-spacing:-.01em}
.aaf-pipe-prog-meta{font-family:var(--mono);font-size:11px;font-weight:600;color:var(--muted);font-variant-numeric:tabular-nums;white-space:nowrap}
.aaf-pipe-prog-track{height:6px;border-radius:999px;background:color-mix(in srgb,var(--border) 75%,transparent);overflow:hidden;width:100%}
.aaf-pipe-prog-fill{height:100%;border-radius:999px;transition:width .25s ease}
.aaf-pipe-now-sub{font-size:11.5px;color:var(--ink2,var(--muted));white-space:nowrap;overflow:hidden;text-overflow:ellipsis}

.aaf-pipe-board{background:var(--surface2);border:1px solid var(--border);border-radius:11px;padding:10px;display:flex;flex-direction:column;gap:8px;min-width:0}
.aaf-pipe-grid{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:8px 14px;width:100%;min-width:0}
@media(max-width:460px){.aaf-pipe-grid{grid-template-columns:repeat(2,minmax(0,1fr));gap:8px 12px}}
.aaf-pipe-cell{position:relative;min-width:0;display:flex}
.aaf-pipe-cell:not(:nth-child(4n)):not(:last-child)::after{content:"›";position:absolute;right:-11px;top:50%;transform:translateY(-52%);font-size:15px;font-weight:700;line-height:1;color:var(--border-strong,var(--muted));pointer-events:none;user-select:none}
.aaf-pipe-cell.edge-ok:not(:nth-child(4n)):not(:last-child)::after{color:var(--ok)}
@media(max-width:460px){
  .aaf-pipe-cell:not(:nth-child(4n)):not(:last-child)::after{display:none}
  .aaf-pipe-cell:not(:nth-child(2n)):not(:last-child)::after{display:block;right:-10px}
}
.aaf-pipe-node{position:relative;flex:1;min-width:0;display:flex;flex-direction:column;gap:3px;padding:8px 8px 7px;border-radius:9px;border:1.5px solid var(--border);background:var(--surface);color:var(--ink);text-align:left;cursor:pointer;outline:none;transition:transform .12s ease,border-color .14s ease,box-shadow .14s ease,opacity .14s ease}
.aaf-pipe-node:hover{border-color:var(--accent);opacity:1;transform:translateY(-1px)}
.aaf-pipe-node:focus-visible{outline:2px solid var(--accent);outline-offset:2px}
.aaf-pipe-node.selected{box-shadow:0 0 0 2px var(--accent)!important;border-color:var(--accent)!important;opacity:1}
.aaf-pipe-node.st-passed{background:color-mix(in srgb,var(--ok-bg) 70%,var(--surface));border-color:color-mix(in srgb,var(--ok) 48%,var(--border))}
.aaf-pipe-node.st-running{background:color-mix(in srgb,var(--info-bg,var(--surface2)) 85%,var(--surface));border-color:var(--info,var(--accent))}
.aaf-pipe-node.st-failed{background:color-mix(in srgb,var(--err-bg) 80%,var(--surface));border-color:var(--err)}
.aaf-pipe-node.st-waiting_human{background:color-mix(in srgb,var(--warn-bg) 80%,var(--surface));border-color:var(--warn)}
.aaf-pipe-node.st-skipped{background:var(--surface2);border-style:dashed;opacity:.72}
.aaf-pipe-node.st-pending{opacity:.62}
.aaf-pipe-node.is-current{opacity:1;border-width:2px;box-shadow:0 2px 10px color-mix(in srgb,var(--accent) 22%,transparent),0 0 0 2.5px color-mix(in srgb,var(--accent) 18%,transparent)}
.aaf-pipe-node.is-current.st-running{box-shadow:0 2px 12px color-mix(in srgb,var(--info,var(--accent)) 28%,transparent),0 0 0 2.5px color-mix(in srgb,var(--info,var(--accent)) 22%,transparent)}
.aaf-pipe-node.is-current.st-waiting_human{box-shadow:0 2px 12px color-mix(in srgb,var(--warn) 26%,transparent),0 0 0 2.5px color-mix(in srgb,var(--warn) 22%,transparent)}
.aaf-pipe-node.is-current.st-failed{box-shadow:0 2px 12px color-mix(in srgb,var(--err) 26%,transparent),0 0 0 2.5px color-mix(in srgb,var(--err) 22%,transparent)}
.aaf-pipe-node.is-current.st-passed{box-shadow:0 2px 10px color-mix(in srgb,var(--ok) 24%,transparent),0 0 0 2.5px color-mix(in srgb,var(--ok) 20%,transparent)}
.aaf-pipe-node-top{display:flex;align-items:center;justify-content:space-between;gap:4px;min-width:0}
.aaf-pipe-step{display:inline-flex;align-items:center;justify-content:center;width:19px;height:19px;border-radius:50%;font-size:10.5px;font-weight:800;font-family:var(--mono);flex:none;background:var(--surface2);color:var(--muted);border:1px solid var(--border)}
.aaf-pipe-node.st-passed .aaf-pipe-step{background:#047857;color:#fff;border-color:#047857}
.aaf-pipe-node.st-running .aaf-pipe-step{background:var(--accent);color:#fff;border-color:var(--accent)}
.aaf-pipe-node.st-failed .aaf-pipe-step{background:var(--danger-btn,#DC2626);color:#fff;border-color:var(--danger-btn,#DC2626)}
.aaf-pipe-node.st-waiting_human .aaf-pipe-step{background:#B45309;color:#fff;border-color:#B45309}
.aaf-pipe-badge{font-size:9.5px;font-weight:800;letter-spacing:.03em;text-transform:uppercase;padding:1px 5px;border-radius:999px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:100%}
.aaf-pipe-badge.b-running{background:var(--accent);color:#fff}
.aaf-pipe-badge.b-waiting_human{background:#B45309;color:#fff}
.aaf-pipe-badge.b-failed{background:var(--danger-btn,#DC2626);color:#fff}
.aaf-pipe-badge.b-passed{background:#047857;color:#fff}
.aaf-pipe-badge.b-skipped{background:var(--slate-bg,var(--surface2));color:var(--slate,var(--muted));border:1px solid var(--border)}
.aaf-pipe-badge.b-next{background:var(--accent-soft,var(--surface2));color:var(--accent);border:1px solid var(--accent-b,var(--border))}
.aaf-pipe-badge.b-rw{background:var(--warn-bg);color:var(--warn);border:1px solid color-mix(in srgb,var(--warn) 38%,transparent);font-family:var(--mono)}
.aaf-pipe-label{font-weight:700;font-size:12px;color:var(--ink);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;margin-top:1px}
.aaf-pipe-meta{font-family:var(--mono);font-size:10.5px;color:var(--muted);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;font-variant-numeric:tabular-nums}
.aaf-pipe-node.st-passed .aaf-pipe-meta{color:var(--ok)}
.aaf-pipe-node.st-running .aaf-pipe-meta{color:var(--info,var(--accent));font-weight:600}
.aaf-pipe-node.st-failed .aaf-pipe-meta{color:var(--err);font-weight:600}
.aaf-pipe-node.st-waiting_human .aaf-pipe-meta{color:var(--warn);font-weight:600}

.aaf-pipe-reworks{display:flex;gap:6px;flex-wrap:wrap;align-items:center;padding-top:4px;border-top:1px dashed color-mix(in srgb,var(--border) 80%,transparent)}
.aaf-pipe-rw-pill{display:inline-flex;align-items:center;gap:4px;padding:2px 8px;border-radius:999px;font-family:var(--mono);font-size:11px;font-weight:700;background:var(--warn-bg);color:var(--warn);border:1px solid color-mix(in srgb,var(--warn) 35%,transparent)}
.aaf-pipe-foot{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap}
.aaf-pipe-legend{display:flex;gap:10px;flex-wrap:wrap;align-items:center;font-size:11px;color:var(--muted)}
.aaf-pipe-leg-item{display:inline-flex;align-items:center;gap:4px}
.aaf-pipe-dot{width:7px;height:7px;border-radius:50%;display:inline-block;flex:none}
.aaf-pipe-hint{font-size:11px;color:var(--muted)}
.aaf-pipe-detail{background:var(--surface);border:1px solid var(--border);border-radius:10px;padding:10px 12px;display:flex;flex-direction:column;gap:6px;font-size:12.5px}
.aaf-pipe-detail-head{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap}
.aaf-pipe-detail-meta{display:flex;gap:10px;flex-wrap:wrap;color:var(--muted);font-size:12px;font-variant-numeric:tabular-nums}
.aaf-pipe-detail-body{color:var(--ink2,var(--ink));white-space:pre-wrap;overflow-wrap:anywhere;line-height:1.45}

/* --- SOP Runs --- */
.aaf-sop-list{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(320px,100%),1fr));gap:10px}
.aaf-sop-card{background:var(--surface);border:1px solid var(--border);border-radius:11px;padding:12px 14px;display:flex;flex-direction:column;gap:9px;box-shadow:var(--shadow-card,none);min-width:0}
.aaf-sop-head{display:flex;align-items:flex-start;justify-content:space-between;gap:8px;flex-wrap:wrap}
.aaf-sop-title{font-weight:700;font-size:13.5px;color:var(--ink);overflow-wrap:anywhere}
.aaf-sop-sub{font-family:var(--mono);font-size:11.5px;color:var(--muted);margin-top:2px;overflow-wrap:anywhere}
.aaf-sop-stages{display:flex;align-items:center;gap:5px;flex-wrap:wrap}
.aaf-sop-arrow{color:var(--muted);font-size:11px;user-select:none}
.aaf-sop-chip{display:inline-flex;align-items:center;gap:5px;padding:4px 9px;min-height:30px;border-radius:999px;border:1px solid var(--border);background:var(--surface2);color:var(--ink);font-size:11.5px;font-weight:600;cursor:pointer;transition:border-color .12s,background .12s}
.aaf-sop-chip:hover{border-color:var(--accent)}
.aaf-sop-chip.active{box-shadow:0 0 0 2px var(--accent-soft,var(--info-bg));border-color:var(--accent)}
.aaf-sop-chip.st-passed{background:var(--ok-bg);color:var(--ok);border-color:color-mix(in srgb,var(--ok) 30%,transparent)}
.aaf-sop-chip.st-running{background:var(--info-bg,var(--surface2));color:var(--info,var(--accent));border-color:color-mix(in srgb,var(--info,var(--accent)) 35%,transparent)}
.aaf-sop-chip.st-failed{background:var(--err-bg);color:var(--err);border-color:color-mix(in srgb,var(--err) 30%,transparent)}
.aaf-sop-chip.st-waiting_human{background:var(--warn-bg);color:var(--warn);border-color:color-mix(in srgb,var(--warn) 35%,transparent)}
.aaf-sop-chip.st-pending,.aaf-sop-chip.st-skipped{background:var(--slate-bg,var(--surface2));color:var(--slate,var(--muted))}
.aaf-sop-art-cnt{font-family:var(--mono);font-size:10.5px;padding:0 5px;border-radius:999px;background:color-mix(in srgb,currentColor 14%,transparent)}
.aaf-sop-stage-info{background:var(--surface2);border:1px solid var(--border);border-radius:8px;padding:8px 10px;font-size:12px;color:var(--ink2,var(--ink));display:flex;flex-direction:column;gap:5px}
.aaf-sop-arts{display:flex;gap:5px;flex-wrap:wrap}
.aaf-art-pill{display:inline-flex;align-items:center;gap:4px;padding:2px 8px;border-radius:6px;background:var(--surface);border:1px solid var(--border);font-family:var(--mono);font-size:11px;color:var(--ink);overflow-wrap:anywhere;max-width:100%}

/* --- Studio Score Gauge & Bottlenecks --- */
.aaf-score-wrap{display:flex;flex-direction:column;gap:16px;min-width:0}
.aaf-score-top{display:grid;grid-template-columns:minmax(200px,240px) minmax(0,1fr);gap:18px;align-items:center}
@media(max-width:740px){.aaf-score-top{grid-template-columns:1fr}}
.aaf-gauge-box{display:flex;flex-direction:column;align-items:center;justify-content:center;gap:8px;padding:12px;border-radius:12px;background:var(--surface2);border:1px solid var(--border)}
.aaf-gauge-svg{display:block;width:156px;height:156px}
.aaf-grade-row{display:flex;align-items:center;gap:7px;flex-wrap:wrap;justify-content:center}
.aaf-grade-pill{display:inline-flex;align-items:center;gap:5px;padding:3px 11px;border-radius:999px;font-size:12.5px;font-weight:700}
.aaf-grade-A{background:var(--ok-bg);color:var(--ok)}
.aaf-grade-B{background:var(--info-bg,var(--surface2));color:var(--info,var(--accent))}
.aaf-grade-C{background:var(--warn-bg);color:var(--warn)}
.aaf-grade-D,.aaf-grade-F{background:var(--err-bg);color:var(--err)}
.aaf-grade-NA{background:var(--surface);color:var(--muted);border:1px solid var(--border)}

.aaf-pillars{display:flex;flex-direction:column;gap:11px;min-width:0}
.aaf-pillar{display:flex;flex-direction:column;gap:4px}
.aaf-pillar.inactive{opacity:.52}
.aaf-pillar-head{display:flex;align-items:baseline;justify-content:space-between;gap:8px;flex-wrap:wrap;font-size:13px}
.aaf-pillar-name{font-weight:600;color:var(--ink);display:inline-flex;align-items:center;gap:6px;flex-wrap:wrap}
.aaf-pillar-wt{font-size:11px;color:var(--muted);font-weight:500;font-variant-numeric:tabular-nums}
.aaf-pillar-val{font-family:var(--mono);font-size:12px;font-weight:700;font-variant-numeric:tabular-nums;color:var(--ink)}
.aaf-pillar-track{height:10px;border-radius:999px;background:var(--surface2);overflow:hidden;border:1px solid color-mix(in srgb,var(--border) 70%,transparent)}
.aaf-pillar.inactive .aaf-pillar-track{background:repeating-linear-gradient(-45deg,var(--surface2),var(--surface2) 6px,var(--border) 6px,var(--border) 8px)}
.aaf-pillar-fill{height:100%;border-radius:999px;transition:width .3s ease}
.aaf-pillar-detail{font-size:12px;color:var(--muted);overflow-wrap:anywhere;line-height:1.4}

.aaf-bn-head{font-size:14px;font-weight:700;color:var(--ink);margin-bottom:2px}
.aaf-bn-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(260px,100%),1fr));gap:10px}
.aaf-bn-card{background:var(--surface2);border:1px solid var(--border);border-radius:11px;padding:12px 13px;display:flex;flex-direction:column;gap:7px;min-width:0}
.aaf-bn-card.sev-high{border-top:3px solid var(--err)}
.aaf-bn-card.sev-medium{border-top:3px solid var(--warn)}
.aaf-bn-card.sev-low{border-top:3px solid var(--info,var(--accent))}
.aaf-bn-top{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap}
.aaf-bn-title{font-weight:700;font-size:13.5px;color:var(--ink);overflow-wrap:anywhere}
.aaf-bn-detail{font-size:12.5px;color:var(--ink2,var(--muted));overflow-wrap:anywhere;line-height:1.45}
.aaf-bn-rec{font-size:12.5px;color:var(--ink);background:var(--surface);border:1px solid var(--border);border-left:3px solid var(--accent);border-radius:8px;padding:7px 9px;overflow-wrap:anywhere;line-height:1.45}
.aaf-bn-ok{padding:11px 13px;border-radius:10px;background:var(--ok-bg);color:var(--ok);font-size:13px;font-weight:600}

.aaf-insuff{display:flex;align-items:center;gap:16px;flex-wrap:wrap;padding:14px 16px;border-radius:12px;background:var(--surface2);border:1px dashed var(--border)}
.aaf-insuff-body{flex:1;min-width:200px}
.aaf-insuff-title{font-weight:700;font-size:15px;color:var(--ink);margin-bottom:4px}
.aaf-insuff-sub{font-size:13px;color:var(--muted);line-height:1.5}

.aaf-st-pill{display:inline-flex;align-items:center;gap:4px;padding:2px 8px;border-radius:999px;font-size:11px;font-weight:700;white-space:nowrap}
.aaf-st-passed{background:var(--ok-bg);color:var(--ok)}
.aaf-st-running{background:var(--info-bg,var(--surface2));color:var(--info,var(--accent))}
.aaf-st-failed{background:var(--err-bg);color:var(--err)}
.aaf-st-waiting_human{background:var(--warn-bg);color:var(--warn)}
.aaf-st-pending,.aaf-st-skipped{background:var(--slate-bg,var(--surface2));color:var(--slate,var(--muted))}
.aaf-pipe-empty{padding:20px 14px;text-align:center;color:var(--muted);border:1px dashed var(--border);border-radius:10px;font-size:13px}
`;
    document.head.appendChild(style);
  }

  function normStatus(raw) {
    const s = String(raw || "").trim().toLowerCase();
    if (s === "passed" || s === "done" || s === "succeeded" || s === "ok" || s === "completed") return "passed";
    if (s === "running" || s === "in_progress" || s === "active" || s === "gating") return "running";
    if (s === "failed" || s === "error" || s === "crashed" || s === "timed_out" || s === "blocked") return "failed";
    if (s === "waiting_human" || s === "waiting" || s === "review" || s === "needs_human" || s === "needs_approval") return "waiting_human";
    if (s === "skipped" || s === "cancelled") return "skipped";
    return "pending";
  }

  function isViLang() {
    return Boolean(typeof window !== "undefined" && window.AafI18n && window.AafI18n.getLang() === "vi");
  }

  function statusVisual(st) {
    const vi = isViLang();
    switch (st) {
      case "passed":
        return { stroke: "var(--ok)", fill: "var(--ok-bg)", text: "var(--ok)", icon: "✓", label: vi ? "Đã qua" : "Passed" };
      case "running":
        return { stroke: "var(--info, var(--accent))", fill: "var(--info-bg, var(--surface2))", text: "var(--info, var(--accent))", icon: "●", label: vi ? "Đang chạy" : "Running" };
      case "failed":
        return { stroke: "var(--err)", fill: "var(--err-bg)", text: "var(--err)", icon: "✕", label: vi ? "Thất bại" : "Failed" };
      case "waiting_human":
        return { stroke: "var(--warn)", fill: "var(--warn-bg)", text: "var(--warn)", icon: "⏸", label: vi ? "Chờ duyệt" : "Waiting" };
      case "skipped":
        return { stroke: "var(--slate, var(--muted))", fill: "var(--surface2)", text: "var(--slate, var(--muted))", icon: "–", label: vi ? "Bỏ qua" : "Skipped" };
      default:
        return { stroke: "var(--border)", fill: "var(--surface)", text: "var(--muted)", icon: "○", label: vi ? "Chờ" : "Pending" };
    }
  }

  function fmtMs(ms) {
    const n = Number(ms);
    if (!Number.isFinite(n) || n <= 0) return "";
    if (n < 1000) return `${Math.round(n)}ms`;
    const secs = n / 1000;
    if (secs < 60) return `${secs < 10 ? secs.toFixed(1) : Math.round(secs)}s`;
    const mins = Math.floor(secs / 60);
    const remS = Math.round(secs % 60);
    if (mins < 60) return remS ? `${mins}m ${remS}s` : `${mins}m`;
    const hrs = Math.floor(mins / 60);
    return `${hrs}h ${mins % 60}m`;
  }

  function fmtAgo(ts) {
    if (ts == null || ts === "") return "";
    let epochSec = Number(ts);
    if (!Number.isFinite(epochSec)) {
      const parsed = Date.parse(String(ts));
      if (!Number.isFinite(parsed)) return String(ts);
      epochSec = parsed / 1000;
    } else if (epochSec > 1e12) {
      epochSec = epochSec / 1000;
    }
    const diff = Math.max(0, Math.round(Date.now() / 1000 - epochSec));
    if (diff < 45) return "vừa xong";
    if (diff < 3600) return `${Math.floor(diff / 60)}p trước`;
    if (diff < 86400) return `${Math.floor(diff / 3600)}g trước`;
    return `${Math.floor(diff / 86400)}ng trước`;
  }

  function clipText(str, maxLen) {
    const s = String(str || "");
    return s.length > maxLen ? s.slice(0, maxLen - 1) + "…" : s;
  }

  function fmtNum(val) {
    const n = Number(val);
    if (!Number.isFinite(n)) return "0";
    return Math.abs(n - Math.round(n)) < 0.05 ? String(Math.round(n)) : n.toFixed(1);
  }

  /* =========================================================================
   * 1. renderTaskPipeline(containerEl, pipelineResp, onNodeSelect, opts)
   * ========================================================================= */
  function findCurrentStageIdx(nodes, taskState) {
    const sts = nodes.map((n) => normStatus(n.status));
    const runIdx = sts.indexOf("running");
    if (runIdx >= 0) return runIdx;
    const waitIdx = sts.indexOf("waiting_human");
    if (waitIdx >= 0) return waitIdx;
    for (let i = sts.length - 1; i >= 0; i--) {
      if (sts[i] === "failed") return i;
    }
    const ts = String(taskState || "").trim().toLowerCase();
    if (ts === "done") return nodes.length - 1;
    let lastDone = -1;
    for (let i = 0; i < sts.length; i++) {
      if (sts[i] === "passed" || sts[i] === "skipped") lastDone = i;
    }
    if (lastDone >= 0 && lastDone + 1 < nodes.length) return lastDone + 1;
    return Math.max(0, lastDone);
  }

  function renderTaskPipeline(containerEl, pipelineResp, onNodeSelect, opts) {
    if (!containerEl) return null;
    injectStyles();
    opts = opts || {};
    const vi = isViLang();

    const resp = pipelineResp || {};
    const nodes = Array.isArray(resp.nodes) ? resp.nodes : [];
    const edges = Array.isArray(resp.edges) ? resp.edges : [];

    if (!nodes.length) {
      containerEl.innerHTML = `<div class="aaf-pipe-empty">${esc(vi ? "Chưa có dữ liệu pipeline cho task này." : "No pipeline data recorded for this task.")}</div>`;
      return null;
    }

    const idxById = new Map();
    nodes.forEach((n, i) => idxById.set(String(n.id), i));

    const reworkEdges = [];
    const reworkCountByFrom = new Map();
    edges.forEach((e) => {
      const fi = idxById.get(String(e.from));
      const ti = idxById.get(String(e.to));
      if (fi == null || ti == null) return;
      const kind = String(e.kind || "next").toLowerCase();
      if (kind === "rework" || ti <= fi) {
        const count = Number(e.count) || 1;
        reworkEdges.push({ fromIdx: fi, toIdx: ti, count });
        reworkCountByFrom.set(fi, (reworkCountByFrom.get(fi) || 0) + count);
      }
    });

    const curIdx = findCurrentStageIdx(nodes, resp.state);
    const curNode = nodes[curIdx] || nodes[0];
    const curSt = normStatus(curNode.status);
    const curVis = statusVisual(curSt);
    const passedCount = nodes.filter((n) => {
      const s = normStatus(n.status);
      return s === "passed" || s === "skipped";
    }).length;
    const pct = Math.min(100, Math.round(((curSt === "running" ? passedCount + 0.5 : passedCount) / nodes.length) * 100));
    const bannerPulsing = curSt === "running" || curSt === "waiting_human";
    const fillColor = curSt === "failed"
      ? "var(--err)"
      : (curSt === "waiting_human"
          ? "var(--warn)"
          : (curSt === "running" ? "var(--info, var(--accent))" : "var(--ok)"));

    const curDur = fmtMs(curNode.duration_ms);
    const subParts = [];
    if (curNode.executor) subParts.push(curNode.executor);
    if (curDur) subParts.push(curDur);
    if (Number(curNode.attempts) > 1) subParts.push(vi ? `${curNode.attempts} lượt` : `${curNode.attempts} attempts`);
    if (curNode.detail) subParts.push(String(curNode.detail).replace(/\s+/g, " ").trim());
    const curSub = subParts.join(" · ");

    const currentBadgeFor = (st) => {
      if (st === "running") return { cls: "b-running", txt: vi ? "Đang chạy" : "LIVE" };
      if (st === "waiting_human") return { cls: "b-waiting_human", txt: vi ? "Chờ bạn" : "WAIT" };
      if (st === "failed") return { cls: "b-failed", txt: vi ? "Đang lỗi" : "FAIL" };
      if (st === "passed") return { cls: "b-passed", txt: vi ? "Hoàn tất" : "DONE" };
      if (st === "skipped") return { cls: "b-skipped", txt: vi ? "Bỏ qua" : "SKIP" };
      return { cls: "b-next", txt: vi ? "Tiếp theo" : "NEXT" };
    };

    const cardsHtml = nodes.map((n, i) => {
      const st = normStatus(n.status);
      const vis = statusVisual(st);
      const isCur = i === curIdx;
      const rwCnt = reworkCountByFrom.get(i) || 0;
      const durTxt = fmtMs(n.duration_ms);
      const attempts = Number(n.attempts) || 0;
      const stepIcon = st === "passed" || st === "failed" || st === "waiting_human" || st === "skipped"
        ? vis.icon
        : String(i + 1);
      const rightBadge = isCur
        ? (() => {
            const b = currentBadgeFor(st);
            return `<span class="aaf-pipe-badge ${b.cls}">${esc(b.txt)}</span>`;
          })()
        : (rwCnt > 0 ? `<span class="aaf-pipe-badge b-rw">↻×${rwCnt}</span>` : "");
      const metaParts = [];
      if (durTxt) metaParts.push(durTxt);
      else metaParts.push(vis.label);
      if (attempts > 1) metaParts.push(`×${attempts}`);
      else if (n.executor && st !== "pending") metaParts.push(clipText(n.executor, 10));
      const metaTxt = metaParts.join(" · ");
      const titleTip = `${i + 1}. ${n.label || n.id} (${vis.label})${durTxt ? " · " + durTxt : ""}${n.detail ? "\n" + n.detail : ""}`;

      return `
        <div class="aaf-pipe-cell ${st === "passed" ? "edge-ok" : ""}" role="listitem">
          <button type="button" class="aaf-pipe-node st-${st}${isCur ? " is-current" : ""}" data-node-idx="${i}" title="${esc(titleTip)}" aria-label="${esc(`${i + 1}. ${n.label || n.id}: ${vis.label}`)}">
            <div class="aaf-pipe-node-top">
              <span class="aaf-pipe-step">${esc(stepIcon)}</span>
              ${rightBadge}
            </div>
            <div class="aaf-pipe-label">${esc(n.label || n.id || "Stage")}</div>
            <div class="aaf-pipe-meta">${esc(metaTxt)}</div>
          </button>
        </div>
      `;
    }).join("");

    const reworksHtml = reworkEdges.length
      ? `<div class="aaf-pipe-reworks">${reworkEdges.map((r) => {
          const fromLbl = nodes[r.fromIdx].label || nodes[r.fromIdx].id;
          const toLbl = nodes[r.toIdx].label || nodes[r.toIdx].id;
          return `<span class="aaf-pipe-rw-pill">↻ ${esc(fromLbl)} → ${esc(toLbl)}${r.count > 1 ? ` ×${r.count}` : ""}</span>`;
        }).join("")}</div>`
      : "";

    const stepPrefix = vi ? `Bước ${curIdx + 1}/${nodes.length}` : `Step ${curIdx + 1}/${nodes.length}`;
    const progLabel = vi ? `${passedCount}/${nodes.length} bước · ${pct}%` : `${passedCount}/${nodes.length} · ${pct}%`;

    const wrap = document.createElement("div");
    wrap.className = "aaf-pipe-wrap";
    wrap.innerHTML = `
      <div class="aaf-pipe-banner st-${curSt}">
        <div class="aaf-pipe-banner-top">
          <div class="aaf-pipe-now">
            <span class="aaf-pipe-live-dot${bannerPulsing ? " pulsing" : ""}" style="background:${curVis.stroke};color:${curVis.stroke}"></span>
            <span class="aaf-pipe-now-title">${esc(stepPrefix)} · ${esc(curNode.label || curNode.id)}</span>
            <span class="aaf-st-pill aaf-st-${curSt}">${esc(curVis.label)}</span>
          </div>
          <span class="aaf-pipe-prog-meta">${esc(progLabel)}</span>
        </div>
        <div class="aaf-pipe-prog-track"><div class="aaf-pipe-prog-fill" style="width:${pct}%;background:${fillColor}"></div></div>
        ${curSub ? `<div class="aaf-pipe-now-sub" title="${esc(curSub)}">${esc(curSub)}</div>` : ""}
      </div>
      <div class="aaf-pipe-board">
        <div class="aaf-pipe-grid" role="list" aria-label="${esc(vi ? "Luồng thực thi" : "Task execution pipeline")}">
          ${cardsHtml}
        </div>
        ${reworksHtml}
      </div>
      <div class="aaf-pipe-foot">
        <div class="aaf-pipe-legend">
          <span class="aaf-pipe-leg-item"><span class="aaf-pipe-dot" style="background:var(--ok)"></span>${esc(vi ? "Đã qua" : "Passed")}</span>
          <span class="aaf-pipe-leg-item"><span class="aaf-pipe-dot" style="background:var(--info,var(--accent))"></span>${esc(vi ? "Đang chạy" : "Running")}</span>
          <span class="aaf-pipe-leg-item"><span class="aaf-pipe-dot" style="background:var(--warn)"></span>${esc(vi ? "Chờ / Rework" : "Waiting / Rework")}</span>
          <span class="aaf-pipe-leg-item"><span class="aaf-pipe-dot" style="background:var(--err)"></span>${esc(vi ? "Lỗi" : "Failed")}</span>
          <span class="aaf-pipe-leg-item"><span class="aaf-pipe-dot" style="background:var(--slate,var(--muted))"></span>${esc(vi ? "Chờ / Bỏ qua" : "Pending / Skipped")}</span>
        </div>
        <span class="aaf-pipe-hint">${esc(vi ? "Bấm từng bước để xem chi tiết" : "Click a stage for details")}</span>
      </div>
      <div class="aaf-pipe-detail-slot"></div>
    `;

    containerEl.innerHTML = "";
    containerEl.appendChild(wrap);

    const detailSlot = wrap.querySelector(".aaf-pipe-detail-slot");
    let selectedIdx = null;

    function selectNode(idx) {
      const node = nodes[idx];
      if (!node) return;
      selectedIdx = selectedIdx === idx ? null : idx;
      wrap.querySelectorAll(".aaf-pipe-node").forEach((el) => {
        el.classList.toggle("selected", Number(el.dataset.nodeIdx) === selectedIdx);
      });
      if (selectedIdx == null) {
        detailSlot.innerHTML = "";
      } else {
        const st = normStatus(node.status);
        const vis = statusVisual(st);
        const durTxt = fmtMs(node.duration_ms);
        const canOpenTab = typeof onNodeSelect === "function";
        const tabLabel = canOpenTab
          ? (typeof opts.getActionLabel === "function"
              ? opts.getActionLabel(node)
              : (node.label || node.id || "Detail"))
          : null;
        detailSlot.innerHTML = `
          <div class="aaf-pipe-detail">
            <div class="aaf-pipe-detail-head">
              <div style="display:flex;align-items:center;gap:7px;flex-wrap:wrap">
                <strong>${esc(`${idx + 1}. ${node.label || node.id}`)}</strong>
                <span class="aaf-st-pill aaf-st-${st}">${esc(vis.label)}</span>
                ${node.kind ? `<span class="mono muted">${esc(node.kind)}</span>` : ""}
              </div>
              <div style="display:flex;align-items:center;gap:6px;flex-wrap:wrap">
                ${tabLabel ? `<button type="button" class="btn-primary btn-sm" data-open-node-tab>${esc(vi ? `Mở tab ${tabLabel} →` : `Open ${tabLabel} →`)}</button>` : ""}
                <button type="button" class="btn-sm" data-close-detail aria-label="Close detail">✕</button>
              </div>
            </div>
            <div class="aaf-pipe-detail-meta">
              ${node.attempts != null ? `<span>${esc(vi ? "Lượt" : "Attempts")}: <b>${esc(node.attempts)}</b></span>` : ""}
              ${durTxt ? `<span>${esc(vi ? "Thời lượng" : "Duration")}: <b>${esc(durTxt)}</b></span>` : ""}
              ${node.executor ? `<span>Executor: <span class="mono">${esc(node.executor)}</span></span>` : ""}
              ${node.account_id ? `<span>${esc(vi ? "Tài khoản" : "Account")}: <span class="mono">${esc(node.account_id)}</span></span>` : ""}
            </div>
            ${node.detail ? `<div class="aaf-pipe-detail-body">${esc(node.detail)}</div>` : `<div class="muted" style="font-size:12px">${esc(vi ? "Chưa có chi tiết bổ sung cho bước này." : "No extra detail recorded for this stage.")}</div>`}
          </div>
        `;
        const openTabBtn = detailSlot.querySelector("[data-open-node-tab]");
        if (openTabBtn && typeof onNodeSelect === "function") {
          openTabBtn.addEventListener("click", () => onNodeSelect(node));
        }
        const closeBtn = detailSlot.querySelector("[data-close-detail]");
        if (closeBtn) {
          closeBtn.addEventListener("click", () => {
            selectedIdx = null;
            wrap.querySelectorAll(".aaf-pipe-node").forEach((el) => el.classList.remove("selected"));
            detailSlot.innerHTML = "";
          });
        }
      }
    }

    wrap.querySelectorAll(".aaf-pipe-node").forEach((el) => {
      el.addEventListener("click", () => selectNode(Number(el.dataset.nodeIdx)));
    });

    return { selectNode };
  }

  /* =========================================================================
   * 2. renderSopRuns(containerEl, sopResp)
   * ========================================================================= */
  function renderSopRuns(containerEl, sopResp) {
    if (!containerEl) return null;
    injectStyles();

    const runs = sopResp && Array.isArray(sopResp.runs) ? sopResp.runs : [];
    if (!runs.length) {
      containerEl.innerHTML = `<div class="aaf-pipe-empty">Chưa có SOP run nào được ghi nhận.</div>`;
      return null;
    }

    const list = document.createElement("div");
    list.className = "aaf-sop-list";

    runs.forEach((run, runIdx) => {
      const stages = Array.isArray(run.stages) ? run.stages : [];
      const runSt = normStatus(run.status);
      const card = document.createElement("article");
      card.className = "aaf-sop-card";

      // Pick default focused stage: first failed/running/waiting, else last stage with summary/artifacts
      let activeStageIdx = stages.findIndex((s) => {
        const st = normStatus(s.status);
        return st === "failed" || st === "running" || st === "waiting_human";
      });
      if (activeStageIdx < 0 && stages.length) {
        for (let i = stages.length - 1; i >= 0; i--) {
          if (stages[i].summary || (Array.isArray(stages[i].artifacts) && stages[i].artifacts.length)) {
            activeStageIdx = i;
            break;
          }
        }
      }

      const allArtifacts = [];
      stages.forEach((s) => {
        if (Array.isArray(s.artifacts)) {
          s.artifacts.forEach((a) => {
            if (a != null && String(a).trim()) allArtifacts.push(String(a));
          });
        }
      });

      function renderCard() {
        const chipsHtml = stages.map((stObj, sIdx) => {
          const st = normStatus(stObj.status);
          const vis = statusVisual(st);
          const arts = Array.isArray(stObj.artifacts) ? stObj.artifacts : [];
          const sep = sIdx > 0 ? `<span class="aaf-sop-arrow" aria-hidden="true">→</span>` : "";
          return `${sep}<button type="button" class="aaf-sop-chip st-${st} ${activeStageIdx === sIdx ? "active" : ""}" data-stage-idx="${sIdx}">
            <span aria-hidden="true">${esc(vis.icon)}</span>
            <span>${esc(stObj.id || ("stage-" + (sIdx + 1)))}</span>
            ${arts.length ? `<span class="aaf-sop-art-cnt" title="${arts.length} artifact(s)">${arts.length}</span>` : ""}
          </button>`;
        }).join("");

        const activeStage = activeStageIdx >= 0 ? stages[activeStageIdx] : null;
        const activeArts = activeStage && Array.isArray(activeStage.artifacts) ? activeStage.artifacts : [];

        card.innerHTML = `
          <div class="aaf-sop-head">
            <div style="min-width:0">
              <div class="aaf-sop-title">${esc(run.sop_name || "SOP Run")}</div>
              <div class="aaf-sop-sub">${esc(run.run_id || ("run-" + (runIdx + 1)))}${run.updated_at ? ` · ${esc(fmtAgo(run.updated_at))}` : ""}</div>
            </div>
            <span class="aaf-st-pill aaf-st-${runSt}">${esc(runSt.replace(/_/g, " "))}</span>
          </div>
          ${stages.length ? `<div class="aaf-sop-stages">${chipsHtml}</div>` : `<div class="muted" style="font-size:12px">No stages recorded.</div>`}
          ${activeStage && (activeStage.summary || activeArts.length) ? `
            <div class="aaf-sop-stage-info">
              <div><strong class="mono">${esc(activeStage.id || "")}</strong>${activeStage.summary ? ` — ${esc(activeStage.summary)}` : ""}</div>
              ${activeArts.length ? `<div class="aaf-sop-arts">${activeArts.map((a) => `<span class="aaf-art-pill">📎 ${esc(a)}</span>`).join("")}</div>` : ""}
            </div>` : ""}
          ${(!activeStage || !activeArts.length) && allArtifacts.length ? `
            <div class="aaf-sop-arts">${allArtifacts.slice(0, 6).map((a) => `<span class="aaf-art-pill">📎 ${esc(a)}</span>`).join("")}${allArtifacts.length > 6 ? `<span class="aaf-art-pill">+${allArtifacts.length - 6}</span>` : ""}</div>` : ""}
        `;

        card.querySelectorAll("[data-stage-idx]").forEach((btn) => {
          btn.addEventListener("click", () => {
            const idx = Number(btn.dataset.stageIdx);
            activeStageIdx = activeStageIdx === idx ? -1 : idx;
            renderCard();
          });
        });
      }

      renderCard();
      list.appendChild(card);
    });

    containerEl.innerHTML = "";
    containerEl.appendChild(list);
    return list;
  }

  /* =========================================================================
   * 3. renderStudioScore(containerEl, scoreObj)
   * ========================================================================= */
  function scoreColor(pct) {
    if (pct >= 90) return "var(--ok)";
    if (pct >= 80) return "var(--info, var(--accent))";
    if (pct >= 70) return "var(--warn)";
    if (pct >= 60) return "color-mix(in srgb, var(--warn) 40%, var(--err))";
    return "var(--err)";
  }

  function gradeClass(grade, score) {
    const g = String(grade || "").trim().toUpperCase();
    if (["A", "B", "C", "D", "F"].includes(g)) return "aaf-grade-" + g;
    if (score >= 90) return "aaf-grade-A";
    if (score >= 80) return "aaf-grade-B";
    if (score >= 70) return "aaf-grade-C";
    if (score >= 60) return "aaf-grade-D";
    return "aaf-grade-F";
  }

  function isViLang() {
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
  const sL = (en, vi) => (isViLang() ? vi : en);

  function severityMeta(raw) {
    const s = String(raw || "").trim().toLowerCase();
    if (s === "critical" || s === "high" || s === "err" || s === "error" || s === "p0") {
      return {
        cls: "sev-high",
        pillCls: "aaf-st-failed",
        label: s === "critical" ? sL("Critical", "Nghiêm trọng") : sL("High", "Cao"),
      };
    }
    if (s === "medium" || s === "med" || s === "warn" || s === "warning" || s === "p1") {
      return { cls: "sev-medium", pillCls: "aaf-st-waiting_human", label: sL("Medium", "Trung bình") };
    }
    return { cls: "sev-low", pillCls: "aaf-st-running", label: sL("Low", "Thấp") };
  }

  function renderPillarsHtml(pillars) {
    if (!Array.isArray(pillars) || !pillars.length) {
      return `<div class="aaf-pipe-empty">${esc(sL("No score pillar data available yet.", "Chưa có dữ liệu trụ cột đánh giá."))}</div>`;
    }
    return `<div class="aaf-pillars">` + pillars.map((p) => {
      const active = p.active !== false;
      const maxS = Math.max(0.001, Number(p.max_score) || 100);
      const val = Math.max(0, Math.min(maxS, Number(p.score) || 0));
      const pct = active ? Math.max(0, Math.min(100, (val / maxS) * 100)) : 0;
      const barColor = scoreColor(pct);
      const wt = p.weight_pct != null ? `${fmtNum(p.weight_pct)}%` : "";

      return `
        <div class="aaf-pillar ${active ? "" : "inactive"}">
          <div class="aaf-pillar-head">
            <span class="aaf-pillar-name">
              ${esc(p.label || p.key || "Pillar")}
              ${wt ? `<span class="aaf-pillar-wt">(${esc(wt)})</span>` : ""}
            </span>
            ${active
              ? `<span class="aaf-pillar-val">${esc(fmtNum(val))} / ${esc(fmtNum(maxS))} <span class="muted" style="font-weight:500">(${Math.round(pct)}%)</span></span>`
              : `<span class="aaf-st-pill aaf-st-pending">${esc(sL("N/A", "Không áp dụng"))}</span>`}
          </div>
          <div class="aaf-pillar-track" role="progressbar" aria-valuenow="${active ? Math.round(pct) : 0}" aria-valuemin="0" aria-valuemax="100" aria-label="${esc(p.label || p.key || "Pillar")}">
            ${active ? `<div class="aaf-pillar-fill" style="width:${pct.toFixed(1)}%;background:${barColor}"></div>` : ""}
          </div>
          ${p.detail ? `<div class="aaf-pillar-detail">${esc(p.detail)}</div>` : ""}
        </div>
      `;
    }).join("") + `</div>`;
  }

  function renderBottlenecksHtml(bottlenecks) {
    const items = Array.isArray(bottlenecks) ? bottlenecks.slice(0, 3) : [];
    if (!items.length) {
      return `<div class="aaf-bn-ok">✓ ${esc(sL("No notable bottlenecks detected in this period.", "Không phát hiện điểm nghẽn nổi bật trong kỳ này."))}</div>`;
    }
    return `
      <div>
        <div class="aaf-bn-head">${esc(sL(`Top ${items.length} priority bottleneck${items.length > 1 ? "s" : ""}`, `Top ${items.length} điểm nghẽn cần ưu tiên`))}</div>
      </div>
      <div class="aaf-bn-grid">
        ${items.map((b) => {
          const sev = severityMeta(b.severity);
          return `
            <article class="aaf-bn-card ${sev.cls}">
              <div class="aaf-bn-top">
                <span class="aaf-st-pill ${sev.pillCls}">${esc(sev.label)}</span>
                ${b.key ? `<span class="mono muted" style="font-size:11px">${esc(b.key)}</span>` : ""}
              </div>
              <div class="aaf-bn-title">${esc(b.title || b.key || sL("Bottleneck", "Điểm nghẽn"))}</div>
              ${b.detail ? `<div class="aaf-bn-detail">${esc(b.detail)}</div>` : ""}
              ${b.recommendation ? `<div class="aaf-bn-rec"><strong>${esc(sL("Recommendation:", "Khuyến nghị:"))}</strong> ${esc(b.recommendation)}</div>` : ""}
            </article>
          `;
        }).join("")}
      </div>
    `;
  }

  function renderStudioScore(containerEl, scoreObj) {
    if (!containerEl) return null;
    injectStyles();

    const s = scoreObj || null;
    const isInsuff = !s || String(s.status || "").toLowerCase() === "insufficient_data" || s.score == null;

    if (isInsuff) {
      const evaluated = s && Number.isFinite(Number(s.evaluated_tasks)) ? Number(s.evaluated_tasks) : 0;
      const pillars = s && Array.isArray(s.pillars) ? s.pillars : [];
      containerEl.innerHTML = `
        <div class="aaf-score-wrap">
          <div class="aaf-insuff">
            <svg class="aaf-gauge-svg" style="width:96px;height:96px;flex:none" viewBox="0 0 120 120" role="img" aria-label="${esc(sL("Insufficient data", "Chưa đủ dữ liệu"))}">
              <circle cx="60" cy="60" r="48" fill="none" stroke="var(--border)" stroke-width="10" stroke-dasharray="6 6" />
              <text x="60" y="66" text-anchor="middle" font-size="24" font-weight="700" font-family="var(--sans)" fill="var(--muted)">—</text>
            </svg>
            <div class="aaf-insuff-body">
              <div class="aaf-insuff-title">${esc(sL("Not enough data for Factory Score", "Chưa đủ dữ liệu để chấm điểm xưởng"))}</div>
              <div class="aaf-insuff-sub">
                ${isViLang()
                  ? `Hiện mới có <strong>${esc(evaluated)}</strong> task được đánh giá trong kỳ này. Khi có thêm task hoàn thành hoặc chạy qua gate, bảng điểm 0–100 cùng top điểm nghẽn sẽ tự động hiển thị tại đây.`
                  : `Only <strong>${esc(evaluated)}</strong> task(s) evaluated in this period. Once more tasks complete or pass the gate, the 0–100 score and top bottlenecks appear here automatically.`}
              </div>
            </div>
          </div>
          ${pillars.length ? renderPillarsHtml(pillars) : ""}
        </div>
      `;
      return containerEl;
    }

    const scoreVal = Math.max(0, Math.min(100, Number(s.score) || 0));
    const grade = String(s.grade || (scoreVal >= 90 ? "A" : scoreVal >= 80 ? "B" : scoreVal >= 70 ? "C" : scoreVal >= 60 ? "D" : "F")).toUpperCase();
    const evaluated = Number.isFinite(Number(s.evaluated_tasks)) ? Number(s.evaluated_tasks) : 0;
    const color = scoreColor(scoreVal);

    const radius = 62;
    const circ = 2 * Math.PI * radius;
    const dashOffset = circ * (1 - scoreVal / 100);

    containerEl.innerHTML = `
      <div class="aaf-score-wrap">
        <div class="aaf-score-top">
          <div class="aaf-gauge-box">
            <svg class="aaf-gauge-svg" viewBox="0 0 160 160" role="img" aria-label="${esc(sL(`Factory Score ${Math.round(scoreVal)} out of 100, Grade ${grade}`, `Điểm xưởng ${Math.round(scoreVal)} trên 100, hạng ${grade}`))}">
              <circle cx="80" cy="80" r="${radius}" fill="none" stroke="var(--border)" stroke-width="13" opacity="0.55" />
              <circle cx="80" cy="80" r="${radius}" fill="none" stroke="${color}" stroke-width="13" stroke-linecap="round"
                stroke-dasharray="${circ.toFixed(2)}" stroke-dashoffset="${dashOffset.toFixed(2)}"
                transform="rotate(-90 80 80)" />
              <text x="80" y="78" text-anchor="middle" font-size="36" font-weight="800" font-family="var(--sans)" fill="var(--ink)">${Math.round(scoreVal)}</text>
              <text x="80" y="101" text-anchor="middle" font-size="13" font-weight="600" font-family="var(--mono)" fill="var(--muted)">/ 100</text>
            </svg>
            <div class="aaf-grade-row">
              <span class="aaf-grade-pill ${gradeClass(grade, scoreVal)}">${esc(sL(`Grade ${grade}`, `Hạng ${grade}`))}</span>
              <span class="muted" style="font-size:12px">${esc(sL(`${evaluated} evaluated tasks`, `${evaluated} task đánh giá`))}</span>
            </div>
          </div>
          <div>
            ${renderPillarsHtml(s.pillars || [])}
          </div>
        </div>
        ${renderBottlenecksHtml(s.bottlenecks || [])}
      </div>
    `;

    return containerEl;
  }

  const api = {
    renderTaskPipeline,
    renderSopRuns,
    renderStudioScore,
  };

  if (typeof window !== "undefined") {
    window.AafPipelineUI = api;
  } else if (typeof globalThis !== "undefined") {
    globalThis.AafPipelineUI = api;
  }
})();
