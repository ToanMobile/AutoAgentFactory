//! Protected-file overwrite/removal checks for `ln` and in-place compressors
//! or decompressors.

use std::path::Path;

use crate::shell::Cmd;
use crate::{Ctx, Denial, paths};

use super::{deny, is_guarded_dir, target};

fn file_name(p: &str) -> Option<&str> {
    Path::new(p).file_name()?.to_str()
}

fn has_word(list: &str, word: &str) -> bool {
    list.split_ascii_whitespace().any(|w| w == word)
}

fn long_is(opt: &str, full: &str, min: usize) -> bool {
    opt.len() >= min && full.starts_with(opt)
}

fn join_target_dir(dir: &str, src: &str) -> Option<String> {
    let fname = file_name(src)?;
    Some(match dir.trim_end_matches('/') {
        "" if dir.starts_with('/') => format!("/{fname}"),
        "" => fname.into(),
        base => format!("{base}/{fname}"),
    })
}

fn parse_ln_args(args: &[String]) -> (Vec<&str>, Vec<&str>, Option<&str>) {
    let (mut pos, mut dirs, mut suffix, mut only_pos) = (Vec::new(), Vec::new(), None, false);
    let mut it = args.iter().map(String::as_str);
    while let Some(arg) = it.next() {
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
            if long_is(opt, "--target-directory", 3) {
                dirs.extend(eq.or_else(|| it.next()));
            } else if long_is(opt, "--suffix", 4) {
                suffix = eq.or_else(|| it.next());
            }
        } else {
            let mut chars = arg[1..].chars();
            while let Some(c) = chars.next() {
                if matches!(c, 't' | 'S') {
                    let r = chars.as_str();
                    let val = (!r.is_empty()).then_some(r).or_else(|| it.next());
                    if c == 't' {
                        dirs.extend(val);
                    } else {
                        suffix = val;
                    }
                    break;
                }
            }
        }
    }
    (pos, dirs, suffix)
}

/// Checks `ln` operands (and `-t`/`--target-directory`, `-S`/`--suffix`, or
/// directory destinations) so links cannot overwrite protected files.
pub(super) fn check_ln(args: &[String], ctx: &Ctx) -> Option<Denial> {
    let (pos, target_dirs, suffix) = parse_ln_args(args);
    let suffix = suffix.filter(|s| !s.is_empty());
    let check = |p: &str| {
        target(p, false, "ln", ctx)
            .or_else(|| suffix.and_then(|s| target(&format!("{p}{s}"), false, "ln", ctx)))
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
            .find_map(|d| target(d, false, "ln", ctx).or_else(|| in_dir(d, &pos)));
    }
    if pos.len() == 1 {
        return file_name(pos[0]).and_then(check);
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
    "zstd unzstd zstdmt pzstd zstdcat|unzstd zstdcat|.zst .zstd .gz .xz .lzma .lz4",
    "lz4 unlz4|unlz4|.lz4",
    "brotli||.br",
    "lzop||.lzo",
];

const ENV_OPTS: &str = "GZIP XZ_OPT XZ_DEFAULTS BZIP BZIP2 ZSTD_CLEVEL ZSTD_NBTHREADS LZ4 BROTLI";
const MAX_CUSTOM_SUFFIXES: usize = 4;
const MAX_CANDIDATES: usize = 1024;

fn find_spec(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    COMPRESSORS.iter().find_map(|row| {
        let (names, rest) = row.split_once('|')?;
        let (decomp, sfx) = rest.split_once('|')?;
        has_word(names, name).then_some((names, decomp, sfx))
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

fn unparseable_long(opt: &str, is_zstd: bool, is_lzop: bool) -> bool {
    let out_min = if is_zstd { 3 } else { 12 };
    long_is(opt, "--filelist", 4)
        || long_is(opt, "--files0", 4)
        || long_is(opt, "--output-dir-flat", out_min)
        || long_is(opt, "--output-dir-mirror", out_min)
        || (is_lzop && (long_is(opt, "--path", 4) || long_is(opt, "--extract", 4)))
}

/// Fail-closed check for compressors/decompressors: denies option-bearing env
/// vars, unparseable options, excessive suffixes/operands, and any protected
/// raw or suffix-adjusted candidate path.
pub(super) fn check_compressor(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    let (name, args) = (cmd.name(), cmd.args());
    let (names, decomp, suffixes) = find_spec(name)?;
    let cant = |what: &str| deny(format!("{name}: {what} cannot be verified"));
    if let Some((var, _)) = cmd.assigns.iter().find(|(k, _)| has_word(ENV_OPTS, k)) {
        return Some(cant(&format!("option env var {var}")));
    }
    let (is_lzop, is_zstd) = (names == "lzop", names.starts_with("zstd"));
    let has_name = is_lzop || names.starts_with("gzip");
    let mut decompressing = has_word(decomp, name);
    let (mut recursive, mut restore_name, mut only_pos) = (false, false, false);
    let (mut candidates, mut custom_suffixes) = (Vec::new(), Vec::new());

    for (i, arg) in args.iter().map(String::as_str).enumerate() {
        let next_arg = || args.get(i + 1).map(String::as_str);
        if only_pos || !arg.starts_with('-') {
            candidates.push(arg);
        } else if arg == "--" {
            only_pos = true;
        } else if arg.starts_with("--") {
            let (opt, eq) = arg
                .split_once('=')
                .map_or((arg, None), |(o, v)| (o, Some(v)));
            candidates.extend(eq);
            if unparseable_long(opt, is_zstd, is_lzop) {
                return Some(cant(opt));
            }
            decompressing |= long_is(opt, "--decompress", 3) || long_is(opt, "--uncompress", 3);
            recursive |= long_is(opt, "--recursive", 5);
            restore_name |= has_name && long_is(opt, "--name", 4);
            if long_is(opt, "--suffix", 4) {
                custom_suffixes.extend(eq.or_else(next_arg).filter(|s| !s.is_empty()));
            }
        } else if arg != "-" {
            let mut chars = arg[1..].chars();
            while let Some(c) = chars.next() {
                match c {
                    'd' => decompressing = true,
                    'r' => recursive = true,
                    'N' if has_name => restore_name = true,
                    'x' | 'P' | 'p' if is_lzop => return Some(cant(&format!("-{c} output path"))),
                    'S' => {
                        let r = chars.as_str();
                        let val = (!r.is_empty()).then_some(r).or_else(next_arg);
                        custom_suffixes.extend(val.filter(|s| !s.is_empty()));
                        break;
                    }
                    'o' => {
                        let r = chars.as_str();
                        if !r.is_empty() {
                            candidates.push(r);
                            candidates.extend(r.strip_prefix('='));
                        }
                        if is_lzop || r.contains(['/', '.']) {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        if custom_suffixes.len() > MAX_CUSTOM_SUFFIXES {
            return Some(cant("too many -S suffixes"));
        }
        if candidates.len() > MAX_CANDIDATES {
            return Some(cant("too many operands"));
        }
    }

    if decompressing && restore_name {
        return Some(cant("-N/--name filename"));
    }

    let check_sfx = |tok: &str, sfx: &str, is_default: bool| {
        let cand = format!("{tok}{sfx}");
        let is_tpl = file_name(tok).is_some_and(|n| {
            matches!(
                paths::fold(n).as_str(),
                ".env.example" | ".env.sample" | ".env.template"
            )
        });
        let skip_tpl = is_default && is_tpl && {
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
