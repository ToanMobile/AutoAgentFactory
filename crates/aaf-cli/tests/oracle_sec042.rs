#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle test for SEC-042: `.github/workflows/release.yml` security invariants.

use std::{fs, path::Path};

fn ind(s: &str) -> usize {
    s.len() - s.trim_start_matches(' ').len()
}
fn skip(s: &str) -> bool {
    s.trim().is_empty() || s.trim_start().starts_with('#')
}
fn unq(s: &str) -> &str {
    let s = s.split(" #").next().unwrap_or(s);
    s.trim().trim_matches(['\'', '"'])
}
fn kv(s: &str) -> Option<(&str, &str)> {
    let (k, v) = s.split_once(':')?;
    (v.is_empty() || v.starts_with(char::is_whitespace)).then_some((k.trim(), v.trim()))
}
fn is_blk(v: &str) -> bool {
    let b = unq(v).as_bytes();
    matches!(b.first(), Some(b'|' | b'>')) && b[1..].iter().all(|c| b"+-0123456789".contains(c))
}

fn entries<'a>(blk: &'a [&'a str], step: bool) -> Vec<(&'a str, &'a [&'a str])> {
    let Some(base) = blk.iter().find(|l| !skip(l)).map(|l| ind(l)) else {
        return Vec::new();
    };
    let (ki, mut i, mut out) = (if step { base + 2 } else { base }, 0, Vec::new());
    while i < blk.len() {
        let (dash, tr) = (step && i == 0, blk[i].trim());
        let raw = if dash {
            tr.trim_start_matches("- ")
        } else {
            tr
        };
        if skip(blk[i]) || (!dash && ind(blk[i]) != ki) {
            i += 1;
            continue;
        }
        if let Some((k, v)) = kv(raw) {
            let (start, lim) = (i, if dash && is_blk(v) { base } else { ki });
            i += 1;
            while i < blk.len() {
                let (d, t) = (ind(blk[i]), blk[i].trim());
                if !skip(blk[i]) && d <= lim && !(!step && d == lim && t.starts_with("- ")) {
                    break;
                }
                i += 1;
            }
            out.push((k, &blk[start..i]));
            continue;
        }
        i += 1;
    }
    out
}

fn by<'a>(m: &[(&str, &'a [&'a str])], k: &str) -> Option<&'a [&'a str]> {
    m.iter().find(|(x, _)| *x == k).map(|(_, v)| *v)
}
fn get<'a>(blk: &'a [&'a str], step: bool, k: &str) -> Option<&'a [&'a str]> {
    by(&entries(if step { blk } else { &blk[1..] }, step), k)
}
fn val<'a>(b: &[&'a str]) -> &'a str {
    let l = b[0].trim();
    kv(l.strip_prefix("- ").unwrap_or(l)).map_or("", |(_, v)| unq(v))
}
fn nested<'a>(blk: &'a [&'a str], step: bool, k1: &str, k2: &str) -> Option<&'a str> {
    get(blk, step, k1).and_then(|b| get(b, false, k2)).map(val)
}

fn items<'a>(blk: &'a [&'a str]) -> Vec<&'a [&'a str]> {
    let Some(si) = blk.iter().find(|l| !skip(l)).map(|l| ind(l)) else {
        return Vec::new();
    };
    let idx: Vec<_> = (0..blk.len())
        .filter(|&i| ind(blk[i]) == si && blk[i].trim().starts_with("- "))
        .collect();
    (0..idx.len())
        .map(|k| &blk[idx[k]..idx.get(k + 1).copied().unwrap_or(blk.len())])
        .collect()
}
fn needs_of<'a>(job: &'a [&'a str]) -> Vec<&'a str> {
    let Some(sec) = get(job, false, "needs") else {
        return Vec::new();
    };
    let u = val(sec).trim_matches(['[', ']']);
    if !u.is_empty() {
        return u.split(',').map(unq).filter(|x| !x.is_empty()).collect();
    }
    items(&sec[1..])
        .into_iter()
        .filter_map(|s| s[0].trim().strip_prefix("- ").map(unq))
        .collect()
}
fn expr_toks(s: &str) -> Vec<&str> {
    let (mut out, mut r) = (Vec::new(), s);
    while let Some(a) = r.find("${{") {
        let Some(b) = r[a + 3..].find("}}") else {
            break;
        };
        let sep = |c: char| !(c.is_ascii_alphanumeric() || "_-.".contains(c));
        out.extend(r[a + 3..a + 3 + b].split(sep));
        r = &r[a + 3 + b + 2..];
    }
    out
}
fn has_inputs(s: &str) -> bool {
    let is_in = |t: &str| t.starts_with("inputs.") || t.starts_with("github.event.inputs.");
    expr_toks(s).into_iter().any(is_in)
}
fn find_ref<'a>(s: &'a str, root: &str) -> Option<(&'a str, &'a str)> {
    expr_toks(s).into_iter().find_map(|tok| {
        let p: Vec<_> = tok.split('.').collect();
        let ok = p.len() >= 4 && p[0] == root && p[2] == "outputs";
        (ok && !p[1].is_empty() && !p[3].is_empty()).then(|| (p[1], p[3]))
    })
}
fn find_uses<'a>(steps: &[&'a [&'a str]], pfx: &str) -> (usize, &'a [&'a str]) {
    let f = |s: &&[&str]| get(s, true, "uses").is_some_and(|b| val(b).starts_with(pfx));
    let (i, s) = steps
        .iter()
        .enumerate()
        .find(|(_, s)| f(s))
        .expect("missing step");
    (i, *s)
}
fn read_yml() -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/release.yml");
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}
fn jobs_of<'a>(lines: &'a [&'a str]) -> Vec<(&'a str, &'a [&'a str])> {
    let top = entries(lines, false);
    let jobs = by(&top, "jobs")
        .map(|s| entries(&s[1..], false))
        .unwrap_or_default();
    assert!(!jobs.is_empty(), "expected jobs in release.yml");
    jobs
}

#[test]
fn release_workflow_never_interpolates_inputs_in_run_blocks() {
    let (text, mut bad) = (read_yml(), Vec::new());
    let lines: Vec<_> = text.lines().collect();
    for (jn, jb) in jobs_of(&lines) {
        if let Some(sb) = get(jb, false, "steps") {
            for st in items(&sb[1..]) {
                if let Some(rb) = get(st, true, "run") {
                    let l = rb[0].trim();
                    let h = kv(l.strip_prefix("- ").unwrap_or(l)).map_or("", |(_, v)| v);
                    let body: &[&str] = if is_blk(h) { &rb[1..] } else { &[h] };
                    for line in body {
                        if has_inputs(line) {
                            bad.push(format!("job `{jn}`: {}", line.trim()));
                        }
                    }
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "SEC-042: inputs in run:\n{}",
        bad.join("\n")
    );
}

#[test]
fn release_build_checkout_pins_resolved_tag_ref_after_validation() {
    let text = read_yml();
    let lines: Vec<_> = text.lines().collect();
    let jobs = jobs_of(&lines);
    let build = by(&jobs, "build").expect("no build job");
    let steps = items(&get(build, false, "steps").expect("no steps")[1..]);
    let (co_i, co) = find_uses(&steps, "actions/checkout");
    let r = nested(co, true, "with", "ref").expect("SEC-042: checkout must set `with.ref`");
    assert!(!has_inputs(r), "SEC-042: raw inputs in ref: {r}");
    let ok_src = |sl: &[&str]| {
        sl.iter().any(|l| l.contains("GITHUB_OUTPUT"))
            && (r.contains("refs/tags/") || sl.iter().any(|l| l.contains("refs/tags/")))
    };
    let via_step = find_ref(r, "steps").is_some_and(|(sid, _)| {
        steps[..co_i]
            .iter()
            .any(|s| get(s, true, "id").map(val) == Some(sid) && ok_src(s))
    });
    let via_job = find_ref(r, "needs").is_some_and(|(jid, key)| {
        needs_of(build).contains(&jid)
            && by(&jobs, jid).is_some_and(|j| {
                nested(j, false, "outputs", key).is_some_and(|v| !has_inputs(v)) && ok_src(j)
            })
    });
    assert!(
        via_step || via_job,
        "SEC-042: ref ({r}) not validated before checkout"
    );
}

#[test]
fn release_publish_uses_validated_job_output_for_tag_name() {
    let text = read_yml();
    let lines: Vec<_> = text.lines().collect();
    let jobs = jobs_of(&lines);
    let pj = by(&jobs, "publish").expect("no publish job");
    let steps = items(&get(pj, false, "steps").expect("no steps")[1..]);
    let (_, rel) = find_uses(&steps, "softprops/action-gh-release");
    let tag = nested(rel, true, "with", "tag_name").expect("no tag_name");
    assert!(!has_inputs(tag), "SEC-042: raw inputs in tag_name: {tag}");
    let (jid, key) = find_ref(tag, "needs")
        .unwrap_or_else(|| panic!("SEC-042: tag_name ({tag}) must use needs.<job>.outputs.<tag>"));
    assert!(
        needs_of(pj).contains(&jid),
        "SEC-042: publish needs `{jid}`"
    );
    let prod = by(&jobs, jid).expect("unknown needs job");
    let out = nested(prod, false, "outputs", key).expect("missing output");
    assert!(!has_inputs(out), "SEC-042: {jid}.{key} forwards raw inputs");
}
