//! Protected-file overwrite/removal checks for `ln` and in-place compressors
//! or decompressors.

use std::path::Path;

use crate::shell::Cmd;
use crate::{Ctx, Denial, paths};

use super::{deny, is_guarded_dir, target};

fn join_target_dir(dir: &str, src: &str) -> Option<String> {
    let fname = Path::new(src).file_name()?.to_str()?;
    let base = dir.trim_end_matches('/');
    Some(match (base.is_empty(), dir.starts_with('/')) {
        (true, true) => format!("/{fname}"),
        (true, false) => fname.to_string(),
        (false, _) => format!("{base}/{fname}"),
    })
}

fn has_word(list: &str, word: &str) -> bool {
    list.split_ascii_whitespace().any(|w| w == word)
}

fn parse_ln_args(args: &[String]) -> (Vec<&str>, Vec<&str>, Option<&str>) {
    let (mut pos, mut dirs, mut suffix, mut only_pos, mut i) =
        (Vec::new(), Vec::new(), None, false, 0);
    while i < args.len() {
        let arg = args[i].as_str();
        let mut step = 1;
        if only_pos || !arg.starts_with('-') || arg == "-" {
            if arg != "-" || only_pos {
                pos.push(arg);
            }
        } else if arg == "--" {
            only_pos = true;
        } else if arg.starts_with("--") {
            let (opt, eq) = arg
                .split_once('=')
                .map_or((arg, None), |(o, v)| (o, Some(v)));
            let is_td = opt.len() >= 3 && "--target-directory".starts_with(opt);
            let is_sfx = opt.len() >= 4 && "--suffix".starts_with(opt);
            if is_td || is_sfx {
                let val = eq.or_else(|| {
                    step = 2;
                    args.get(i + 1).map(String::as_str)
                });
                if is_td {
                    dirs.extend(val);
                } else {
                    suffix = val;
                }
            }
        } else {
            let mut chars = arg[1..].chars();
            while let Some(c) = chars.next() {
                if matches!(c, 't' | 'S') {
                    let rest = chars.as_str();
                    let val = (!rest.is_empty()).then_some(rest).or_else(|| {
                        step = 2;
                        args.get(i + 1).map(String::as_str)
                    });
                    if c == 't' {
                        dirs.extend(val);
                    } else {
                        suffix = val;
                    }
                    break;
                }
            }
        }
        i += step;
    }
    (pos, dirs, suffix)
}

/// Checks `ln` operands (and `-t`/`--target-directory`, `-S`/`--suffix`, or
/// directory destinations) so links cannot overwrite protected files.
pub(super) fn check_ln(args: &[String], ctx: &Ctx) -> Option<Denial> {
    let (pos, target_dirs, suffix) = parse_ln_args(args);
    let check = |p: &str| {
        target(p, false, "ln", ctx).or_else(|| {
            suffix
                .filter(|s| !s.is_empty())
                .and_then(|s| target(&format!("{p}{s}"), false, "ln", ctx))
        })
    };
    let in_dir = |d: &str, srcs: &[&str]| {
        srcs.iter()
            .find_map(|s| join_target_dir(d, s).and_then(|j| check(&j)))
    };
    if let Some(d) = pos.iter().find_map(|p| check(p)) {
        return Some(d);
    }
    if !target_dirs.is_empty() {
        return target_dirs
            .into_iter()
            .find_map(|dir| target(dir, false, "ln", ctx).or_else(|| in_dir(dir, &pos)));
    }
    if pos.len() == 1 {
        return Path::new(pos[0])
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(check);
    }
    let (&dst, srcs) = pos.split_last()?;
    let dst_path = ctx.expand_real_dotdot(dst);
    let is_dir = dst.ends_with('/')
        || matches!(dst, "." | "..")
        || srcs.len() > 1
        || is_guarded_dir(&dst_path)
        || dst_path.is_dir();
    is_dir.then(|| in_dir(dst, srcs)).flatten()
}

// Rows: "names|default-decompress names|default suffixes"
const COMPRESSORS: &[&str] = &[
    "gzip gunzip pigz unpigz|gunzip unpigz|.gz -gz .z -z _z .Z .zz",
    "bzip2 bunzip2 pbzip2 pbunzip2 lbzip2 lbunzip2|bunzip2 pbunzip2 lbunzip2|.bz2 .bz",
    "xz unxz lzma unlzma|unxz unlzma|.xz .lzma",
    "lzip||.lz",
    "compress uncompress|uncompress|.Z",
    "zstd unzstd zstdmt|unzstd|.zst .zstd .gz .xz .lzma .lz4",
    "lz4 unlz4|unlz4|.lz4",
    "brotli||.br",
    "lzop||.lzo",
];

const ENV_OPTS: &str = "GZIP XZ_OPT XZ_DEFAULTS BZIP BZIP2 ZSTD_CLEVEL ZSTD_NBTHREADS LZ4 BROTLI";

fn find_spec(name: &str) -> Option<(&'static str, &'static str)> {
    COMPRESSORS.iter().find_map(|row| {
        let mut parts = row.splitn(3, '|');
        let names = parts.next()?;
        has_word(names, name).then(|| (parts.next().unwrap_or(""), parts.next().unwrap_or("")))
    })
}

pub(super) fn is_compressor(name: &str) -> bool {
    find_spec(name).is_some()
}

fn strip_suffix_ci<'a>(arg: &'a str, sfx: &str) -> Option<&'a str> {
    let (len, bytes) = (sfx.len(), arg.as_bytes());
    (len > 0
        && bytes.len() > len
        && bytes[bytes.len() - len..].eq_ignore_ascii_case(sfx.as_bytes())
        && arg.is_char_boundary(arg.len() - len))
    .then(|| &arg[..arg.len() - len])
}

fn is_env_template(raw: &str) -> bool {
    Path::new(raw)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| {
            matches!(
                paths::fold(n).as_str(),
                ".env.example" | ".env.sample" | ".env.template"
            )
        })
}

fn unparseable_long(opt: &str, is_zstd: bool, is_lzop: bool) -> bool {
    (opt.len() >= 4 && ("--filelist".starts_with(opt) || "--files0".starts_with(opt)))
        || ((is_zstd && opt.len() >= 3 || opt.starts_with("--output-dir"))
            && ("--output-dir-flat".starts_with(opt) || "--output-dir-mirror".starts_with(opt)))
        || (is_lzop
            && opt.len() >= 4
            && ("--path".starts_with(opt) || "--extract".starts_with(opt)))
}

/// Fail-closed check for compressors/decompressors: treats every non-flag
/// token (and `--opt=VALUE` / attached `-oVALUE`) as a potential path, checks
/// raw and suffix-added/stripped variants, and denies unparseable options or
/// option-bearing env assignments when operands are present.
pub(super) fn check_compressor(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    let (name, args) = (cmd.name(), cmd.args());
    let (decomp, suffixes) = find_spec(name)?;
    let (is_lzop, is_zstd) = (name == "lzop", matches!(name, "zstd" | "unzstd" | "zstdmt"));
    let has_name = matches!(name, "gzip" | "gunzip" | "pigz" | "unpigz" | "lzop");
    let mut decompressing = has_word(decomp, name);
    let (mut recursive, mut restore_name, mut has_operand, mut only_pos) =
        (false, false, false, false);
    let (mut candidates, mut custom_suffixes) = (Vec::new(), Vec::new());

    for (i, arg) in args.iter().map(String::as_str).enumerate() {
        if only_pos || !arg.starts_with('-') {
            has_operand = true;
            candidates.push(arg);
            continue;
        }
        if arg == "--" {
            only_pos = true;
            continue;
        }
        if arg == "-" {
            continue;
        }
        if arg.starts_with("--") {
            let (opt, eq) = arg
                .split_once('=')
                .map_or((arg, None), |(o, v)| (o, Some(v)));
            candidates.extend(eq);
            if unparseable_long(opt, is_zstd, is_lzop) {
                return Some(deny(format!("{name}: {opt} cannot be verified")));
            }
            decompressing |= opt.len() >= 3
                && ("--decompress".starts_with(opt) || "--uncompress".starts_with(opt));
            recursive |= opt.len() >= 5 && "--recursive".starts_with(opt);
            restore_name |= has_name && opt.len() >= 4 && "--name".starts_with(opt);
            if opt.len() >= 4 && "--suffix".starts_with(opt) {
                let val = eq.or_else(|| args.get(i + 1).map(String::as_str));
                custom_suffixes.extend(val.filter(|s| !s.is_empty()));
            }
            continue;
        }
        let mut chars = arg[1..].chars();
        while let Some(c) = chars.next() {
            match c {
                'd' => decompressing = true,
                'r' => recursive = true,
                'N' if has_name => restore_name = true,
                'x' | 'P' | 'p' if is_lzop => {
                    return Some(deny(format!("{name}: -{c} output path cannot be verified")));
                }
                'S' => {
                    let rest = chars.as_str();
                    let val = (!rest.is_empty())
                        .then_some(rest)
                        .or_else(|| args.get(i + 1).map(String::as_str));
                    custom_suffixes.extend(val.filter(|s| !s.is_empty()));
                    break;
                }
                'o' => {
                    let rest = chars.as_str();
                    if !rest.is_empty() {
                        candidates.push(rest);
                        candidates.extend(rest.strip_prefix('='));
                    }
                    if is_lzop || rest.contains(['/', '.']) {
                        break;
                    }
                }
                _ => {}
            }
        }
    }

    if decompressing && restore_name {
        return Some(deny(format!(
            "{name}: -N/--name filename cannot be verified"
        )));
    }
    if has_operand && let Some((var, _)) = cmd.assigns.iter().find(|(k, _)| has_word(ENV_OPTS, k)) {
        return Some(deny(format!(
            "{name}: option env var {var} cannot be verified"
        )));
    }

    let check_sfx = |tok: &str, sfx: &str, is_default: bool| {
        let cand = format!("{tok}{sfx}");
        let skip_tpl = is_default && is_env_template(tok) && {
            let p = ctx.expand_real_dotdot(&cand);
            p.parent()
                .zip(p.file_name())
                .is_some_and(|(d, f)| paths::real(d).join(f) == paths::real(&p))
        };
        (!skip_tpl)
            .then(|| target(&cand, false, name, ctx))
            .flatten()
            .or_else(|| strip_suffix_ci(tok, sfx).and_then(|o| target(o, false, name, ctx)))
    };

    candidates.into_iter().find_map(|tok| {
        target(tok, recursive, name, ctx).or_else(|| {
            suffixes
                .split(' ')
                .map(|s| (s, true))
                .chain(custom_suffixes.iter().map(|&s| (s, false)))
                .find_map(|(s, is_default)| check_sfx(tok, s, is_default))
        })
    })
}
