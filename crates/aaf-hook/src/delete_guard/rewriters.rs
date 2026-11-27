//! Protected-file overwrite/removal checks for `ln` and in-place compressors
//! or decompressors.

use std::path::Path;

use crate::{Ctx, Denial};

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

/// Long-option match from `known` (`=` takes arg, `!` output-dir, `?` file-list).
struct LongHit<'a>(&'a str, bool, bool, bool, bool); // (name, exact, takes_val, unp_out, unp_in)

enum Opt<'a> {
    Long(LongHit<'a>),
    Short(char),
}

/// Resolves `--opt` by exact match or unique prefix (`getopt_long`). Ambiguous
/// prefixes where every candidate is unparseable still fail closed.
fn match_long<'a>(opt: &'a str, known: &'static str) -> Option<LongHit<'a>> {
    if opt.len() <= 2 {
        return None;
    }
    let (mut hit, mut n, mut all_out, mut all_in, mut any_val) = (None, 0usize, true, true, false);
    for tok in known.split_ascii_whitespace() {
        let name = tok.trim_end_matches(['=', '!', '?']);
        let (tv, uo, ui) = (tok.contains('='), tok.ends_with('!'), tok.ends_with('?'));
        if name == opt {
            return Some(LongHit(name, true, tv, uo, ui));
        }
        if name.starts_with(opt) {
            hit = Some(LongHit(name, false, tv, uo, ui));
            (n, all_out, all_in, any_val) = (n + 1, all_out && uo, all_in && ui, any_val || tv);
        }
    }
    match n {
        1 => hit,
        2.. if all_out || all_in => Some(LongHit(opt, false, any_val, all_out, all_in)),
        _ => None,
    }
}

fn walk_args<'a>(
    args: &'a [String],
    known_long: &'static str,
    valued_short: &str,
    mut on_opt: impl FnMut(Opt<'a>, Option<&'a str>),
) -> Vec<&'a str> {
    let (mut pos, mut only_pos, mut i) = (Vec::new(), false, 0);
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
            if let Some(mut hit) = match_long(opt, known_long) {
                hit.1 &= eq.is_none();
                let val = if hit.2 && eq.is_none() {
                    step = 2;
                    args.get(i + 1).map(String::as_str)
                } else {
                    eq
                };
                on_opt(Opt::Long(hit), val);
            }
        } else {
            let mut chars = arg[1..].chars();
            while let Some(c) = chars.next() {
                if valued_short.contains(c) {
                    let rest = chars.as_str();
                    let val = if rest.is_empty() {
                        step = 2;
                        args.get(i + 1).map(String::as_str)
                    } else {
                        Some(rest)
                    };
                    on_opt(Opt::Short(c), val);
                    break;
                }
                on_opt(Opt::Short(c), None);
            }
        }
        i += step;
    }
    pos
}

const LN_LONG: &str = "--suffix= --symbolic --target-directory=";

/// Checks `ln` operands (and `-t`/`--target-directory`, `-S`/`--suffix`, or
/// directory destinations) so links cannot overwrite protected files.
pub(super) fn check_ln(args: &[String], ctx: &Ctx) -> Option<Denial> {
    let (mut target_dirs, mut suffix) = (Vec::new(), None);
    let pos = walk_args(args, LN_LONG, "tS", |opt, v| match opt {
        Opt::Long(LongHit("--target-directory", ..)) | Opt::Short('t') => target_dirs.extend(v),
        Opt::Long(LongHit("--suffix", ..)) | Opt::Short('S') => suffix = v,
        _ => {}
    });
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

// Rows: "names|decomp|flags|rm|suffixes[first=default]|valued_short|known_long"
// flags: D=deletes_default k=keep r=recursive N=name S=suffix o=output 2=pos2_out p=path_p
const SPECS: &[&str] = &[
    "gzip gunzip pigz unpigz|gunzip unpigz|DkrNS||.gz -gz .z -z _z .Z .zz|S|\
     --decompress --keep --name --no-name --recursive --rsyncable --stdout --suffix= \
     --synchronous --test --to-stdout --uncompress",
    "bzip2 bunzip2 pbzip2 pbunzip2 lbzip2 lbunzip2|bunzip2 pbunzip2 lbunzip2|Dk||.bz2 .bz||\
     --compress --decompress --keep --small --stdout --test --to-stdout --uncompress",
    "xz unxz lzma unlzma|unxz unlzma|DkS||.xz .lzma|SFCMT|\
     --check= --compress --decompress --files? --files0? --force --format= --keep --memlimit= \
     --memlimit-compress= --memlimit-decompress= --stdout --suffix= --test --threads= \
     --to-stdout --uncompress",
    "lzip||Dko||.lz|bmosS|\
     --decompress --dictionary-size= --keep --match-length= --member-size= --output= --stdout \
     --volume-size=",
    "compress uncompress|uncompress|Dr||.Z|b|",
    "zstd unzstd zstdmt|unzstd|kro|--rm|.zst .zstd .gz .xz .lzma .lz4|oD|\
     --check --compress --decompress --fast --filelist=? --force --format= --keep --memory= \
     --output-dir-flat=! --output-dir-mirror=! --rm --rsyncable --single-thread --stdout \
     --threads= --uncompress",
    "lz4 unlz4|unlz4|kr2|--rm|.lz4|D|\
     --compress --decompress --keep --multiple --recursive --rm --stdout --test --to-stdout \
     --uncompress",
    "brotli||kSo|j --rm|.br|oSqwD|\
     --decompress --dictionary= --keep --lgwin= --output= --quality= --repeat --rm --stdout \
     --suffix=",
    "lzop||kNSop|U --unlink --delete|.lzo|oS|\
     --archive-name= --decompress --delete --filter= --keep --name --no-name --output= --path=! \
     --stdout --suffix= --test --to-stdout --uncompress --unlink",
];

fn find_spec(name: &str) -> Option<&'static str> {
    SPECS
        .iter()
        .copied()
        .find(|r| has_word(r.split('|').next().unwrap_or(""), name))
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

/// Checks compressors/decompressors against protected files and guarded dirs.
pub(super) fn check_compressor(name: &str, args: &[String], ctx: &Ctx) -> Option<Denial> {
    let mut c = find_spec(name)?.splitn(7, '|');
    let [
        _,
        decomp,
        flags,
        rm_flags,
        suffixes,
        valued_short,
        known_long,
    ] = std::array::from_fn(|_| c.next().unwrap_or(""));
    let has = |ch: char| flags.contains(ch);
    let has_compress = has_word(known_long, "--compress");
    let (mut to_stdout, mut keeps, mut has_rm) = (false, false, false);
    let (mut recursive, mut restore_name, mut multiple) = (false, false, false);
    let (mut unp_out, mut unp_in) = (false, false);
    let mut decompressing = has_word(decomp, name);
    let (mut custom_suffix, mut output_files) = (None, Vec::new());

    let positionals = walk_args(args, known_long, valued_short, |opt, val| {
        if let Opt::Long(h) = &opt {
            (unp_out, unp_in) = (unp_out || h.3, unp_in || h.4);
        }
        match opt {
            Opt::Long(LongHit("--stdout" | "--to-stdout", true, ..)) | Opt::Short('c') => {
                to_stdout = true;
            }
            Opt::Long(LongHit("--keep", true, ..)) | Opt::Short('k') if has('k') => keeps = true,
            Opt::Long(LongHit("--decompress" | "--uncompress", ..)) | Opt::Short('d') => {
                decompressing = true;
            }
            Opt::Long(LongHit("--compress", ..)) => decompressing = false,
            Opt::Short('z') if has_compress => decompressing = false,
            Opt::Long(LongHit("--recursive", ..)) | Opt::Short('r') if has('r') => recursive = true,
            Opt::Long(LongHit("--name", ..)) | Opt::Short('N') if has('N') => restore_name = true,
            Opt::Long(LongHit("--multiple", ..)) | Opt::Short('m') if has('2') => multiple = true,
            Opt::Long(LongHit("--suffix", ..)) | Opt::Short('S') if has('S') => custom_suffix = val,
            Opt::Long(LongHit("--output", ..)) | Opt::Short('o') if has('o') => {
                output_files.extend(val);
            }
            Opt::Short('p') if has('p') => unp_out = true,
            Opt::Long(h) if has_word(rm_flags, h.0) => has_rm = true,
            Opt::Short(ch) if has_word(rm_flags, ch.encode_utf8(&mut [0; 4])) => has_rm = true,
            _ => {}
        }
    });

    let default_sfx = match name {
        "lzma" | "unlzma" => ".lzma",
        _ => suffixes.split(' ').next().unwrap_or(""),
    };
    let suffix = custom_suffix.unwrap_or(default_sfx);
    if let Some(d) = output_files.iter().find_map(|out| {
        target(out, false, name, ctx).or_else(|| {
            (!decompressing)
                .then(|| target(&format!("{out}{suffix}"), false, name, ctx))
                .flatten()
        })
    }) {
        return Some(d);
    }
    if unp_out {
        return Some(deny(format!(
            "{name}: output-directory option cannot be verified"
        )));
    }
    if has('2')
        && !multiple
        && !recursive
        && let Some(&out) = positionals.get(1)
        && let Some(d) = target(out, false, name, ctx)
    {
        return Some(d);
    }
    if to_stdout {
        return None;
    }
    if unp_in {
        return Some(deny(format!("{name}: file-list option cannot be verified")));
    }
    if decompressing && restore_name {
        return Some(deny(format!(
            "{name}: -N/--name filename cannot be verified"
        )));
    }

    let removes_input = if has('D') { !keeps } else { has_rm };
    positionals.iter().find_map(|pos| {
        if (removes_input || recursive)
            && let Some(d) = target(pos, recursive, name, ctx)
        {
            return Some(d);
        }
        if decompressing {
            custom_suffix
                .and_then(|s| strip_suffix_ci(pos, s))
                .or_else(|| suffixes.split(' ').find_map(|s| strip_suffix_ci(pos, s)))
                .and_then(|o| target(o, false, name, ctx))
        } else {
            target(&format!("{pos}{suffix}"), false, name, ctx)
        }
    })
}
