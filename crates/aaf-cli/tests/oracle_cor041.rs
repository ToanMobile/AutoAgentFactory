#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn walk_prod_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if path.is_dir() {
            if !["target", ".git", ".claude", "tests", "benches"].contains(&name.as_ref()) {
                walk_prod_rs(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs")
            && name != "tests.rs"
            && !name.ends_with("_tests.rs")
        {
            out.push(path);
        }
    }
}

fn collect_compile_time_includes(root: &Path) -> BTreeSet<String> {
    let mut rs_files = Vec::new();
    for entry in fs::read_dir(root.join("crates")).unwrap() {
        let dir = entry.unwrap().path();
        if dir.join("src").is_dir() {
            walk_prod_rs(&dir.join("src"), &mut rs_files);
        }
        if dir.join("build.rs").is_file() {
            rs_files.push(dir.join("build.rs"));
        }
    }
    let mut deps = BTreeSet::new();
    for rs_path in rs_files {
        let src = fs::read_to_string(&rs_path).unwrap();
        let cleaned = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let parent = rs_path.parent().unwrap();
        for macro_name in ["include_str!", "include_bytes!"] {
            let mut rest = cleaned.as_str();
            while let Some(idx) = rest.find(macro_name) {
                let prev = rest[..idx].chars().next_back();
                rest = &rest[idx + macro_name.len()..];
                if prev.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                if let Some(lit) = rest
                    .trim_start()
                    .strip_prefix('(')
                    .and_then(|s| s.trim_start().strip_prefix('"'))
                    .and_then(|s| s.split_once('"').map(|(lit, _)| lit))
                {
                    let abs = parent.join(lit).canonicalize().unwrap_or_else(|e| {
                        panic!("resolve {lit:?} from {}: {e}", rs_path.display())
                    });
                    let rel = abs.strip_prefix(root).unwrap().to_string_lossy();
                    deps.insert(rel.replace('\\', "/"));
                }
            }
        }
    }
    deps
}

type CopyEntry = (String, String, bool); // (src, dst, dst_is_dir)

fn norm_rel(p: &str) -> String {
    match p.trim().trim_start_matches("./").trim_matches('/') {
        "." | "" => String::new(),
        other => other.to_string(),
    }
}

fn parse_builder_copies_before_cargo_build(dockerfile: &str) -> Vec<CopyEntry> {
    let mut lines = Vec::new();
    let mut buf = String::new();
    for raw in dockerfile.lines().map(str::trim) {
        if buf.is_empty() && (raw.is_empty() || raw.starts_with('#')) {
            continue;
        }
        if let Some(cont) = raw.strip_suffix('\\') {
            buf.push_str(cont.trim_end());
            buf.push(' ');
        } else {
            buf.push_str(raw);
            lines.push(std::mem::take(&mut buf));
        }
    }
    let (mut in_builder, mut saw_build, mut workdir) = (false, false, String::from("/"));
    let mut copies = Vec::new();
    for line in lines {
        let (instr, rest) = line.split_once(char::is_whitespace).unwrap_or((&line, ""));
        let (instr, rest) = (instr.to_ascii_uppercase(), rest.trim());
        match instr.as_str() {
            "FROM" if in_builder => break,
            "FROM" => {
                let toks: Vec<&str> = rest.split_whitespace().collect();
                in_builder = toks.windows(2).any(|w| {
                    w[0].eq_ignore_ascii_case("AS") && w[1].eq_ignore_ascii_case("builder")
                });
            }
            _ if !in_builder => {}
            "WORKDIR" => {
                let t = rest.trim_end_matches('/');
                workdir = if t.is_empty() { "/".into() } else { t.into() };
            }
            "RUN" if rest.contains("cargo build") => {
                saw_build = true;
                break;
            }
            "COPY" | "ADD" => {
                let toks: Vec<String> = if rest.starts_with('[') {
                    serde_json::from_str(rest).unwrap()
                } else {
                    rest.split_whitespace().map(str::to_string).collect()
                };
                let nflags = toks.iter().take_while(|t| t.starts_with("--")).count();
                let (flags, args) = toks.split_at(nflags);
                if flags.iter().any(|f| f.starts_with("--from=")) || args.len() < 2 {
                    continue;
                }
                let (srcs, raw_dst) = (&args[..args.len() - 1], &args[args.len() - 1]);
                let dst_is_dir = raw_dst.ends_with('/')
                    || matches!(raw_dst.as_str(), "." | "./")
                    || *raw_dst == workdir;
                let wd = workdir.trim_end_matches('/');
                let dst = if !raw_dst.starts_with('/') {
                    Some(norm_rel(raw_dst))
                } else if raw_dst.trim_end_matches('/') == wd {
                    Some(String::new())
                } else {
                    raw_dst.strip_prefix(&format!("{wd}/")).map(norm_rel)
                };
                if let Some(dst) = dst {
                    for src in srcs {
                        copies.push((norm_rel(src), dst.clone(), dst_is_dir));
                    }
                }
            }
            _ => {}
        }
    }
    assert!(in_builder, "Dockerfile must define a `builder` stage");
    assert!(
        saw_build,
        "Dockerfile `builder` stage must run `cargo build`"
    );
    copies
}

fn copy_covers_dep(root: &Path, (src, dst, dst_is_dir): &CopyEntry, dep: &str) -> bool {
    if src.is_empty() {
        return dst.is_empty();
    }
    if root.join(src).is_dir() {
        return dep.starts_with(&format!("{src}/")) && dst == src;
    }
    if src != dep {
        return false;
    }
    let name = Path::new(src).file_name().unwrap().to_string_lossy();
    let landed = match (*dst_is_dir, dst.is_empty()) {
        (false, _) => dst.clone(),
        (true, true) => name.into_owned(),
        (true, false) => format!("{dst}/{name}"),
    };
    landed == dep
}

#[test]
fn cor041_dockerfile_builder_copies_all_compile_time_include_assets() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let dockerfile = fs::read_to_string(root.join("packaging/docker/Dockerfile")).unwrap();
    let copies = parse_builder_copies_before_cargo_build(&dockerfile);
    let includes = collect_compile_time_includes(&root);
    assert!(
        includes.iter().any(|d| !d.starts_with("crates/")),
        "expected compile-time include_str!/include_bytes! assets outside crates/ (e.g. templates/)"
    );
    let missing: Vec<&str> = includes
        .iter()
        .filter(|dep| !copies.iter().any(|c| copy_covers_dep(&root, c, dep)))
        .map(String::as_str)
        .collect();
    assert!(
        missing.is_empty(),
        "packaging/docker/Dockerfile builder stage does not COPY all compile-time included assets \
         before `RUN cargo build`.\nBuilder COPY entries: {copies:?}\nMissing assets ({}):\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}
