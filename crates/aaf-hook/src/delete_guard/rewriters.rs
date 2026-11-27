//! Protected-file overwrite/removal checks for `ln` and in-place compressors.

use std::path::Path;

use crate::shell::Cmd;
use crate::{Ctx, Denial, paths};

use super::{GUARDED_DIRS, deny, is_guarded_dir, target};

const MAX_MODIFIERS: usize = 4;
const MAX_CANDIDATES: usize = 1024;
const ENV_OPTS: &str = "GZIP XZ_OPT XZ_DEFAULTS BZIP BZIP2 LZ4 BROTLI PIGZ LZOP LBZIP2";

// Rows: "names|default-decompress names|default suffixes"
const COMPRESSORS: &[&str] = &[
    "gzip gunzip pigz unpigz bgzip zforce znew gzexe|gunzip unpigz|.gz -gz .z -z _z .Z .zz",
    "bzip2 bunzip2 pbzip2 pbunzip2 lbzip2 lbunzip2 bzip3 bunzip3|bunzip2 pbunzip2 lbunzip2 \
     bunzip3|.bz2 .bz .bz3",
    "xz unxz lzma unlzma pxz pixz|unxz unlzma|.xz .lzma .pxz .tpxz",
    "lzip plzip lunzip|lunzip|.lz",
    "compress uncompress|uncompress|.Z",
    "zstd unzstd zstdmt pzstd zstdcat|unzstd zstdcat|.zst .zstd .gz .xz .lzma .lz4",
    "lz4 unlz4 lz4c|unlz4|.lz4",
    "brotli||.br",
    "lzop||.lzo",
    "rzip runzip lrzip lrunzip lrzcat|runzip lrunzip lrzcat|.rz .lrz",
];

fn cant(name: &str, what: &str) -> Denial {
    deny(format!("{name}: {what} cannot be verified"))
}

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
    Some(format!("{}/{}", dir.trim_end_matches('/'), file_name(src)?))
}

/// Pure string rule: a `/`- or `=`-separated part names a guarded directory
/// or a protected file, whichever option or operand it sits in.
fn names_protected(s: &str) -> bool {
    s.split(['/', '=']).any(|c| {
        GUARDED_DIRS.contains(&paths::fold(c).as_str())
            || paths::protected_name(Path::new(c)).is_some()
    })
}

/// Every string that may be a path, without option semantics (even `--` may
/// be an option's value): each argument, values after `=`, and each tail of a
/// short-option cluster.
fn candidates(args: &[String]) -> Option<Vec<&str>> {
    let mut out = Vec::new();
    for arg in args.iter().map(String::as_str) {
        out.push(arg);
        if arg.starts_with('-') {
            out.extend(arg.split_once('=').map(|(_, v)| v));
            if !arg.starts_with("--") {
                out.extend(arg.char_indices().skip(2).map(|(i, _)| &arg[i..]));
            }
        }
        if out.len() > MAX_CANDIDATES {
            return None;
        }
    }
    Some(out)
}

/// Candidates of `cmd`, denied when too many or when one (or an assigned
/// value) names a protected path.
fn named_candidates(cmd: &Cmd) -> Result<Vec<&str>, Denial> {
    let name = cmd.name();
    let toks = candidates(cmd.args()).ok_or_else(|| cant(name, "too many operands"))?;
    let values = cmd.assigns.iter().map(|(_, v)| v.as_str());
    let bad = toks
        .iter()
        .copied()
        .chain(values)
        .find(|t| names_protected(t));
    bad.map_or(Ok(toks), |t| {
        Err(deny(format!("{name} {t}: names a protected path")))
    })
}

/// Checks every `ln` candidate, and its name inside each directory candidate.
pub(super) fn check_ln(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    let backup = cmd
        .assigns
        .iter()
        .any(|(k, _)| matches!(k.as_str(), "SIMPLE_BACKUP_SUFFIX" | "VERSION_CONTROL"))
        || cmd.args().iter().any(|a| {
            a.starts_with("--su") || (!a.starts_with("--") && a.starts_with('-') && a.contains('S'))
        });
    if backup {
        return Some(cant("ln", "backup suffix"));
    }
    let pos = match named_candidates(cmd) {
        Ok(pos) => pos,
        Err(denial) => return Some(denial),
    };
    let is_dir = |d: &&str| {
        let p = ctx.expand_real_dotdot(d);
        d.ends_with('/') || matches!(*d, "." | "..") || is_guarded_dir(&p) || p.is_dir()
    };
    let dirs: Vec<&str> = pos.iter().copied().filter(is_dir).collect();
    if dirs.len() > MAX_MODIFIERS {
        return Some(cant("ln", "more than 4 target directories"));
    }
    let in_dir = |d: &str| {
        pos.iter()
            .find_map(|s| join_target_dir(d, s).and_then(|j| target(&j, false, "ln", ctx)))
    };
    pos.iter()
        .find_map(|p| target(p, paths::has_glob(p), "ln", ctx))
        .or_else(|| dirs.into_iter().chain(["."]).find_map(in_dir))
}

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
    let split = arg.len().checked_sub(sfx.len())?;
    (!sfx.is_empty()
        && split > 0
        && arg.is_char_boundary(split)
        && arg[split..].eq_ignore_ascii_case(sfx))
    .then(|| &arg[..split])
}

fn unparseable_long(opt: &str, is_zstd: bool, is_lzop: bool) -> bool {
    let out_min = if is_zstd { 3 } else { 12 };
    long_is(opt, "--filelist", 4)
        || long_is(opt, "--files0", 4)
        || long_is(opt, "--outdir", 6)
        || long_is(opt, "--output-dir-flat", out_min)
        || long_is(opt, "--output-dir-mirror", out_min)
        || (is_lzop && (long_is(opt, "--path", 4) || long_is(opt, "--extract", 4)))
}

/// Fail-closed check for compressors/decompressors on env vars, options, and paths.
pub(super) fn check_compressor(cmd: &Cmd, ctx: &Ctx) -> Option<Denial> {
    let (name, args) = (cmd.name(), cmd.args());
    let (names, decomp, suffixes) = find_spec(name)?;
    if let Some((var, _)) = cmd.assigns.iter().find(|(k, _)| has_word(ENV_OPTS, k)) {
        return Some(cant(name, &format!("option env var {var}")));
    }
    let (is_lzop, is_zstd) = (names == "lzop", names.starts_with("zstd"));
    let has_name = is_lzop || names.starts_with("gzip");
    let is_rz = names.starts_with("rzip");
    let mut decompressing = has_word(decomp, name);
    let (mut recursive, mut restore_name, mut custom_suffixes) = (false, false, Vec::new());

    let flags = args.iter().map(String::as_str).enumerate();
    for (i, arg) in flags.filter(|(_, a)| a.len() > 1 && a.starts_with('-')) {
        let next_arg = || args.get(i + 1).map(String::as_str);
        if arg.starts_with("--") {
            let (opt, eq) = arg
                .split_once('=')
                .map_or((arg, None), |(o, v)| (o, Some(v)));
            if unparseable_long(opt, is_zstd, is_lzop) {
                return Some(cant(name, opt));
            }
            decompressing |= long_is(opt, "--decompress", 3) || long_is(opt, "--uncompress", 3);
            recursive |= long_is(opt, "--recursive", 5);
            restore_name |= has_name && long_is(opt, "--name", 4);
            if long_is(opt, "--suffix", 4) {
                custom_suffixes.extend(eq.or_else(next_arg).filter(|s| !s.is_empty()));
            }
        } else {
            let mut chars = arg[1..].chars();
            while let Some(c) = chars.next() {
                match c {
                    'd' => decompressing = true,
                    'r' => recursive = true,
                    'N' if has_name => restore_name = true,
                    'x' | 'P' | 'p' if is_lzop => return Some(cant(name, "-x/-p/-P output")),
                    'O' if is_rz => return Some(cant(name, "-O output directory")),
                    'S' => {
                        let r = chars.as_str();
                        let val = (!r.is_empty()).then_some(r).or_else(next_arg);
                        custom_suffixes.extend(val.filter(|s| !s.is_empty()));
                        break;
                    }
                    'o' => break,
                    _ => {}
                }
            }
        }
        if custom_suffixes.len() > MAX_MODIFIERS {
            return Some(cant(name, "too many suffixes"));
        }
    }
    if decompressing && restore_name {
        return Some(cant(name, "-N/--name filename"));
    }
    let toks = match named_candidates(cmd) {
        Ok(toks) => toks,
        Err(denial) => return Some(denial),
    };

    // Suffix variants of a glob stay string-only: the `tok*` walk covers them.
    let fs_check = |p: &str| (!paths::has_glob(p)).then(|| target(p, false, name, ctx))?;
    let variant = |tok: &str, sfx: &str, is_default: bool| {
        let added = format!("{tok}{sfx}");
        // `tok` passed `names_protected`, so an `.env.*` name here is a template.
        let is_tpl = file_name(tok).is_some_and(|n| paths::fold(n).starts_with(".env."));
        let skip_tpl = is_default && is_tpl && {
            let p = ctx.expand_real_dotdot(&added);
            p.parent()
                .zip(p.file_name())
                .is_some_and(|(d, f)| paths::real(d).join(f) == paths::real(&p))
        };
        let stripped = strip_suffix_ci(tok, sfx);
        if (!skip_tpl && names_protected(&added)) || stripped.is_some_and(names_protected) {
            return Some(deny(format!("{name} {tok}: may name a protected path")));
        }
        (!skip_tpl)
            .then(|| fs_check(&added))
            .flatten()
            .or_else(|| stripped.and_then(fs_check))
    };

    toks.into_iter().find_map(|tok| {
        let glob_tail = (paths::has_glob(tok) && !tok.ends_with('*')).then(|| format!("{tok}*"));
        target(tok, recursive, name, ctx)
            .or_else(|| glob_tail.and_then(|g| target(&g, false, name, ctx)))
            .or_else(|| {
                suffixes
                    .split(' ')
                    .map(|s| (s, true))
                    .chain(custom_suffixes.iter().map(|&s| (s, false)))
                    .find_map(|(s, is_default)| variant(tok, s, is_default))
            })
    })
}
