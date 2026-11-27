//! Protected-file overwrite/removal checks for `ln` and in-place compressors
//! or decompressors (`gzip`, `bzip2`, `xz`, `lzma`, `lzip`, `compress`, `zstd`,
//! and their `gunzip`/`bunzip2`/`unxz`/`unlzma`/`uncompress` counterparts).

use std::path::Path;

use crate::{Ctx, Denial};

use super::{is_guarded_dir, target};

fn join_target_dir(dir: &str, src: &str) -> Option<String> {
    let fname = Path::new(src).file_name()?.to_str()?;
    let base = dir.trim_end_matches('/');
    if base.is_empty() {
        Some(if dir.starts_with('/') {
            format!("/{fname}")
        } else {
            fname.to_string()
        })
    } else {
        Some(format!("{base}/{fname}"))
    }
}

/// Checks `ln` operands (and `-t`/`--target-directory` or directory destinations)
/// so symlinks/hardlinks cannot overwrite or link onto protected files.
pub(super) fn check_ln(args: &[String], ctx: &Ctx) -> Option<Denial> {
    let mut target_dir: Option<&str> = None;
    let mut positionals: Vec<&str> = Vec::new();
    let mut only_pos = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if only_pos || !arg.starts_with('-') || arg == "-" {
            positionals.push(arg);
            i += 1;
            continue;
        }
        if arg == "--" {
            only_pos = true;
            i += 1;
            continue;
        }
        if let Some(dir) = arg.strip_prefix("--target-directory=") {
            target_dir = Some(dir);
            i += 1;
            continue;
        }
        if arg == "--target-directory" {
            target_dir = args.get(i + 1).map(String::as_str);
            i += 2;
            continue;
        }
        if arg == "--suffix" {
            i += 2;
            continue;
        }
        if !arg.starts_with("--") && arg.len() > 1 {
            let stop = arg[1..].find('S');
            let prefix = stop.map_or(&arg[1..], |p| &arg[1..1 + p]);
            if let Some(pos) = prefix.find('t') {
                let rest = &arg[2 + pos..];
                if rest.is_empty() {
                    target_dir = args.get(i + 1).map(String::as_str);
                    i += 2;
                } else {
                    target_dir = Some(rest);
                    i += 1;
                }
                continue;
            }
            if let Some(pos) = stop
                && 1 + pos == arg.len() - 1
            {
                i += 2;
                continue;
            }
        }
        i += 1;
    }

    for pos in &positionals {
        if let Some(denial) = target(pos, false, "ln", ctx) {
            return Some(denial);
        }
    }

    if let Some(dir) = target_dir {
        if let Some(denial) = target(dir, false, "ln", ctx) {
            return Some(denial);
        }
        for src in &positionals {
            if let Some(joined) = join_target_dir(dir, src)
                && let Some(denial) = target(&joined, false, "ln", ctx)
            {
                return Some(denial);
            }
        }
        return None;
    }

    if positionals.len() == 1 {
        if let Some(fname) = Path::new(positionals[0])
            .file_name()
            .and_then(|n| n.to_str())
        {
            return target(fname, false, "ln", ctx);
        }
        return None;
    }

    let (&dst, sources) = positionals.split_last()?;
    let dst_path = ctx.expand_real_dotdot(dst);
    let dst_is_dir = dst.ends_with('/')
        || matches!(dst, "." | "..")
        || sources.len() > 1
        || is_guarded_dir(&dst_path)
        || dst_path.is_dir();
    if dst_is_dir {
        for src in sources {
            if let Some(joined) = join_target_dir(dst, src)
                && let Some(denial) = target(&joined, false, "ln", ctx)
            {
                return Some(denial);
            }
        }
    }
    None
}

fn compressor_valued_short(name: &str) -> &'static str {
    match name {
        "gzip" | "gunzip" => "S",
        "xz" | "unxz" | "lzma" | "unlzma" => "SFCMT",
        "lzip" => "bmosS",
        "compress" | "uncompress" => "b",
        "zstd" => "oD",
        _ => "",
    }
}

fn compressor_valued_long(name: &str, arg: &str) -> bool {
    match name {
        "gzip" | "gunzip" => arg == "--suffix",
        "xz" | "unxz" | "lzma" | "unlzma" => matches!(
            arg,
            "--suffix"
                | "--format"
                | "--check"
                | "--memlimit"
                | "--memlimit-compress"
                | "--memlimit-decompress"
                | "--threads"
        ),
        "lzip" => matches!(
            arg,
            "--output" | "--member-size" | "--volume-size" | "--dictionary-size" | "--match-length"
        ),
        _ => false,
    }
}

fn strip_compressed_suffix<'a>(
    arg: &'a str,
    name: &str,
    custom_suffix: Option<&str>,
) -> Option<&'a str> {
    if let Some(sfx) = custom_suffix.filter(|s| !s.is_empty())
        && let Some(stripped) = arg.strip_suffix(sfx)
        && !stripped.is_empty()
    {
        return Some(stripped);
    }
    let suffixes: &[&str] = match name {
        "gzip" | "gunzip" => &[".gz", "-gz", ".z", "-z", "_z", ".Z"],
        "bzip2" | "bunzip2" => &[".bz2", ".bz"],
        "xz" | "unxz" => &[".xz", ".lzma"],
        "lzma" | "unlzma" => &[".lzma", ".xz"],
        "lzip" => &[".lz"],
        "compress" | "uncompress" => &[".Z"],
        "zstd" => &[".zst", ".zstd"],
        _ => &[],
    };
    let bytes = arg.as_bytes();
    suffixes.iter().find_map(|sfx| {
        let len = sfx.len();
        (bytes.len() > len && bytes[bytes.len() - len..].eq_ignore_ascii_case(sfx.as_bytes()))
            .then(|| &arg[..arg.len() - len])
    })
}

/// Checks compressors and decompressors that remove or overwrite protected
/// files in place unless kept (`-k`/`--keep`) or sent to stdout
/// (`-c`/`--stdout`/`--to-stdout`). `zstd` removes inputs only with `--rm`.
pub(super) fn check_compressor(name: &str, args: &[String], ctx: &Ctx) -> Option<Denial> {
    let mut to_stdout = false;
    let mut keeps = false;
    let mut has_rm = false;
    let mut decompressing = matches!(
        name,
        "gunzip" | "bunzip2" | "unxz" | "unlzma" | "uncompress"
    );
    let mut custom_suffix: Option<&str> = None;
    let mut output_files: Vec<&str> = Vec::new();
    let mut positionals: Vec<&str> = Vec::new();
    let mut only_pos = false;
    let valued_short = compressor_valued_short(name);

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if only_pos || !arg.starts_with('-') || arg == "-" {
            if arg != "-" || only_pos {
                positionals.push(arg);
            }
            i += 1;
            continue;
        }
        if arg == "--" {
            only_pos = true;
            i += 1;
            continue;
        }
        if arg.starts_with("--") {
            match arg {
                "--stdout" | "--to-stdout" => to_stdout = true,
                "--keep" => keeps = true,
                "--rm" => has_rm = true,
                "--decompress" | "--uncompress" => decompressing = true,
                "--output" if matches!(name, "lzip" | "zstd") => {
                    if let Some(out) = args.get(i + 1).map(String::as_str) {
                        output_files.push(out);
                    }
                    i += 2;
                    continue;
                }
                "--suffix" if compressor_valued_long(name, arg) => {
                    custom_suffix = args.get(i + 1).map(String::as_str);
                    i += 2;
                    continue;
                }
                _ => {
                    if let Some(sfx) = arg.strip_prefix("--suffix=") {
                        custom_suffix = Some(sfx);
                    } else if let Some(out) = arg
                        .strip_prefix("--output=")
                        .filter(|_| matches!(name, "lzip" | "zstd"))
                    {
                        output_files.push(out);
                    } else if compressor_valued_long(name, arg) {
                        i += 2;
                        continue;
                    }
                }
            }
            i += 1;
            continue;
        }

        let mut chars = arg[1..].chars();
        let mut consumed_next = false;
        while let Some(c) = chars.next() {
            if valued_short.contains(c) {
                let rest = chars.as_str();
                let val = if rest.is_empty() {
                    consumed_next = true;
                    args.get(i + 1).map(String::as_str)
                } else {
                    Some(rest)
                };
                if c == 'S' && matches!(name, "gzip" | "gunzip" | "xz" | "unxz" | "lzma" | "unlzma")
                {
                    custom_suffix = val;
                } else if c == 'o'
                    && matches!(name, "lzip" | "zstd")
                    && let Some(out) = val
                {
                    output_files.push(out);
                }
                break;
            }
            match c {
                'c' => to_stdout = true,
                'k' => keeps = true,
                'd' if name != "compress" => decompressing = true,
                _ => {}
            }
        }
        i += if consumed_next { 2 } else { 1 };
    }

    for out in output_files {
        if let Some(denial) = target(out, false, name, ctx) {
            return Some(denial);
        }
    }

    if to_stdout {
        return None;
    }

    let removes_input = if name == "zstd" { has_rm } else { !keeps };
    for pos in &positionals {
        if removes_input && let Some(denial) = target(pos, false, name, ctx) {
            return Some(denial);
        }
        if decompressing
            && let Some(out) = strip_compressed_suffix(pos, name, custom_suffix)
            && let Some(denial) = target(out, false, name, ctx)
        {
            return Some(denial);
        }
    }
    None
}
