//! Pure parsers & timestamp helpers for live non-Claude provider `/usage`
//! responses (`agy -p=/usage --output-format json`/TSV and Grok CLI JSON/PTY).

use serde_json::{Value, json};

/// Converts an RFC3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ` or with `.fracZ`)
/// to Unix epoch milliseconds. Returns `None` for invalid or sentinel (`<= 0`) timestamps.
pub(crate) fn parse_rfc3339_utc_ms(raw: &str) -> Option<i64> {
    let s = raw.trim().strip_suffix('Z')?;
    let (date, time_raw) = s.split_once('T')?;
    let time = time_raw.split('.').next()?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let m: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if d.next().is_some() {
        return None;
    }
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mm: i64 = t.next()?.parse().ok()?;
    let ss: i64 = t.next()?.parse().ok()?;
    if t.next().is_some() {
        return None;
    }
    if !(1970..=2100).contains(&y)
        || !(1..=12).contains(&m)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hh)
        || !(0..=59).contains(&mm)
        || !(0..=59).contains(&ss)
    {
        return None;
    }
    // Howard Hinnant's civil-from-days inverse algorithm (UTC).
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    (secs > 0).then_some(secs * 1000)
}

fn format_remain_human(reset_at_ms: i64, now_epoch_ms: i64) -> String {
    let diff = (reset_at_ms - now_epoch_ms) / 1000;
    if diff <= 0 {
        return String::new();
    }
    let days = diff / 86_400;
    let hours = (diff % 86_400) / 3600;
    let mins = (diff % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins:02}m")
    } else {
        format!("{mins}m")
    }
}

fn build_agy_payload(
    rem5: Option<i64>,
    reset5_ms: Option<i64>,
    rem7: Option<i64>,
    reset7_ms: Option<i64>,
    groups: Vec<Value>,
    fetched_ms: i64,
) -> Option<Value> {
    if rem5.is_none() && rem7.is_none() {
        return None;
    }
    let reset5_human = reset5_ms
        .map(|ms| format_remain_human(ms, fetched_ms))
        .unwrap_or_default();
    let reset7_human = reset7_ms
        .map(|ms| format_remain_human(ms, fetched_ms))
        .unwrap_or_default();
    Some(json!({
        "live": true,
        "source": "agy_usage",
        "remainingPct": rem5,
        "resetAtMs": reset5_ms,
        "resetRemainHuman": reset5_human,
        "weeklyRemainingPct": rem7,
        "weeklyResetAtMs": reset7_ms,
        "weeklyResetFormatted": reset7_human,
        "groups": groups,
        "fetchedAtMs": fetched_ms,
    }))
}

fn parse_pct_value(tok: &str) -> Option<f64> {
    let v: f64 = tok.trim().parse().ok()?;
    (v.is_finite() && (0.0..=100.0).contains(&v)).then_some(v)
}

fn merge_min_window(
    slot_pct: &mut Option<i64>,
    slot_reset: &mut Option<i64>,
    pct: i64,
    ts: Option<i64>,
) {
    match *slot_pct {
        Some(prev) if prev < pct => {}
        Some(prev) if prev == pct => {
            if ts > *slot_reset {
                *slot_reset = ts;
            }
        }
        _ => {
            *slot_pct = Some(pct);
            *slot_reset = ts;
        }
    }
}

/// Parses JSON or tab-separated output from `agy -p=/usage --mode plan`.
pub(crate) fn parse_agy_usage_output(raw: &str, fetched_ms: i64) -> Option<Value> {
    if let Ok(val) = serde_json::from_str::<Value>(raw.trim()) {
        if let Some(groups) = val
            .pointer("/command/data/groups")
            .and_then(Value::as_array)
        {
            let (mut rem5, mut reset5_ms) = (None, None);
            let (mut rem7, mut reset7_ms) = (None, None);
            let mut groups_out = Vec::new();
            for g in groups {
                let g_name = g["name"].as_str().unwrap_or("").trim();
                let is_gemini = g_name.to_ascii_lowercase().contains("gemini");
                let (mut g5, mut g7) = (None, None);
                if let Some(buckets) = g["buckets"].as_array() {
                    for b in buckets {
                        let pct = b["remaining_fraction"]
                            .as_f64()
                            .filter(|f| f.is_finite() && (0.0..=1.0).contains(f))
                            .map(|f| {
                                let r = (f * 100.0).round() as i64;
                                if f > 0.0 { r.clamp(1, 100) } else { 0 }
                            });
                        let Some(pct_val) = pct else { continue };
                        let win = b["window"].as_str().unwrap_or("").to_ascii_lowercase();
                        let bname = b["name"].as_str().unwrap_or("").to_ascii_lowercase();
                        let ts = b["reset_time"].as_str().and_then(parse_rfc3339_utc_ms);
                        if win == "5h" || bname.contains("five hour") || bname.contains("5h") {
                            g5 = Some(pct_val);
                            if is_gemini {
                                merge_min_window(&mut rem5, &mut reset5_ms, pct_val, ts);
                            }
                        } else if win == "weekly" || bname.contains("weekly") || bname.contains("7d")
                        {
                            g7 = Some(pct_val);
                            if is_gemini {
                                merge_min_window(&mut rem7, &mut reset7_ms, pct_val, ts);
                            }
                        }
                    }
                }
                if !g_name.is_empty() && (g5.is_some() || g7.is_some()) {
                    groups_out.push(json!({
                        "name": g_name,
                        "remainingPct": g5,
                        "weeklyRemainingPct": g7,
                    }));
                }
            }
            if let Some(parsed) =
                build_agy_payload(rem5, reset5_ms, rem7, reset7_ms, groups_out, fetched_ms)
            {
                return Some(parsed);
            }
        }
        if let Some(resp) = val["response"].as_str() {
            return parse_agy_usage_tsv(resp, fetched_ms);
        }
    }
    parse_agy_usage_tsv(raw, fetched_ms)
}

fn parse_agy_usage_tsv(raw: &str, fetched_ms: i64) -> Option<Value> {
    let (mut rem5, mut reset5_ms) = (None, None);
    let (mut rem7, mut reset7_ms) = (None, None);
    for line in raw.lines() {
        let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
        if cols.len() < 3 || !cols[0].to_ascii_lowercase().contains("gemini") {
            continue;
        }
        let label = cols[1].to_ascii_lowercase();
        let Some((num_tok, _)) = cols[2].split_once('%') else {
            continue;
        };
        let Some(pct_f) = parse_pct_value(num_tok) else {
            continue;
        };
        let pct_val = if pct_f > 0.0 {
            (pct_f.round() as i64).clamp(1, 100)
        } else {
            0
        };
        let ts_ms = cols.get(3).and_then(|ts| parse_rfc3339_utc_ms(ts));
        if label.contains("five hour") || label.contains("5h") {
            merge_min_window(&mut rem5, &mut reset5_ms, pct_val, ts_ms);
        } else if label.contains("weekly") || label.contains("7d") {
            merge_min_window(&mut rem7, &mut reset7_ms, pct_val, ts_ms);
        }
    }
    build_agy_payload(rem5, reset5_ms, rem7, reset7_ms, Vec::new(), fetched_ms)
}

pub(super) fn parse_grok_usage_json(raw: &str, fetched_ms: i64) -> Option<Value> {
    let val: Value = serde_json::from_str(raw.trim()).ok()?;
    let rem5 = val
        .get("remainingPct")
        .or_else(|| val.get("fiveHourRemainingPct"))
        .or_else(|| val.get("five_hour_remaining_pct"))
        .and_then(Value::as_i64)
        .filter(|v| (0..=100).contains(v));
    let rem7 = val
        .get("weeklyRemainingPct")
        .or_else(|| val.get("weekly_remaining_pct"))
        .and_then(Value::as_i64)
        .filter(|v| (0..=100).contains(v));
    if rem5.is_none() && rem7.is_none() {
        return None;
    }
    Some(json!({
        "live": true,
        "source": "grok_usage",
        "remainingPct": rem5,
        "weeklyRemainingPct": rem7,
        "fetchedAtMs": fetched_ms,
    }))
}

fn used_pct_to_remaining(used: f64) -> i64 {
    let rem = 100.0 - used;
    if rem <= 0.0 {
        0
    } else {
        (rem.round() as i64).clamp(1, 100)
    }
}

/// Parses interactive `/usage` output from Grok TUI (`Weekly limit: 6%`,
/// `Next reset: December 2, 11:40`, and optional `5-hour limit: N%`).
/// Grok reports `% used`, so remaining is `100 - used`.
pub(crate) fn parse_grok_pty_usage(raw: &str, fetched_ms: i64) -> Option<Value> {
    let mut rem5: Option<i64> = None;
    let mut rem7: Option<i64> = None;
    let mut next_reset = String::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        let low = trimmed.to_ascii_lowercase();
        if let Some(pos) = low.find("weekly limit:") {
            let after = &trimmed[pos + "weekly limit:".len()..];
            if let Some((pct_tok, _)) = after.split_once('%')
                && let Some(used) = parse_pct_value(pct_tok)
            {
                rem7 = Some(used_pct_to_remaining(used));
            }
        }
        for label in ["five-hour limit:", "5-hour limit:", "5h limit:"] {
            if let Some(pos) = low.find(label) {
                let after = &trimmed[pos + label.len()..];
                if let Some((pct_tok, _)) = after.split_once('%')
                    && let Some(used) = parse_pct_value(pct_tok)
                {
                    rem5 = Some(used_pct_to_remaining(used));
                }
            }
        }
        if let Some(pos) = low.find("next reset:") {
            let after = trimmed[pos + "next reset:".len()..].trim();
            if !after.is_empty() {
                next_reset = after.to_string();
            }
        }
    }
    if rem5.is_none() && rem7.is_none() {
        return None;
    }
    Some(json!({
        "live": true,
        "source": "grok_pty_usage",
        "remainingPct": rem5,
        "weeklyRemainingPct": rem7,
        "weeklyResetFormatted": next_reset,
        "fetchedAtMs": fetched_ms,
    }))
}
