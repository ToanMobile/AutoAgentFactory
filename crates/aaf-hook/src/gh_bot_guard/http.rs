//! Blocks direct HTTP client and interpreter write calls to
//! `api.github.com` or `uploads.github.com`.

use super::deny;
use crate::{Denial, interp_guard, pr_merge_guard, shell};

pub(super) fn check_http(cmd: &shell::Cmd) -> Option<Denial> {
    let name = cmd.name();
    let is_interp = interp_guard::is_interpreter(name) || matches!(name, "deno" | "bun");
    let is_http = pr_merge_guard::HTTP_CLIENTS.contains(&name);
    if !is_interp && !is_http {
        return None;
    }

    let mut texts: Vec<String> = cmd.args().to_vec();
    texts.extend(pr_merge_guard::stdin_texts(cmd));

    let has_gh_url = texts
        .iter()
        .flat_map(|text| pr_merge_guard::urls(text))
        .any(is_github_api_url);
    if !has_gh_url {
        return None;
    }

    let is_write = if is_interp {
        interp_has_write(&texts)
    } else {
        match name {
            "curl" => curl_is_write(cmd.args()),
            "wget" | "wget2" => wget_is_write(cmd.args()),
            "lwp-request" => lwp_is_write(cmd.args()),
            "http" | "https" | "httpie" | "xh" | "xhs" => httpie_is_write(cmd.args()),
            _ => generic_http_is_write(cmd.args()),
        }
    };

    is_write.then(|| {
        deny(format!(
            "direct HTTP write via `{name}` to GitHub API bypasses bot marker enforcement"
        ))
    })
}

fn is_github_api_url(url: &str) -> bool {
    let after_scheme = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => return false,
    };
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host_port.split_once(':').map_or(host_port, |(h, _)| h);
    host.eq_ignore_ascii_case("api.github.com") || host.eq_ignore_ascii_case("uploads.github.com")
}

fn is_read_method(method: &str) -> bool {
    let m = method.trim();
    m.eq_ignore_ascii_case("GET") || m.eq_ignore_ascii_case("HEAD")
}

fn interp_has_write(texts: &[String]) -> bool {
    texts.iter().any(|text| {
        let lower = text.to_ascii_lowercase();
        lower.contains("post")
            || lower.contains("patch")
            || lower.contains("put")
            || lower.contains("delete")
    })
}

fn curl_is_write(args: &[String]) -> bool {
    let mut i = 0;
    while let Some(arg) = args.get(i).map(String::as_str) {
        if arg == "--" {
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, glued) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            match name {
                "request" => {
                    let method = glued.or_else(|| args.get(i + 1).map(String::as_str));
                    if method.is_none_or(|m| !is_read_method(m)) {
                        return true;
                    }
                    i += if glued.is_some() { 1 } else { 2 };
                }
                "json" | "form" | "form-string" | "upload-file" => return true,
                n if n == "data" || n.starts_with("data-") => return true,
                _ => i += 1,
            }
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            let cluster = &arg[1..];
            let mut step = 1;
            for (offset, ch) in cluster.char_indices() {
                let rem = &cluster[offset + ch.len_utf8()..];
                match ch {
                    'd' | 'F' | 'T' => return true,
                    'X' => {
                        let method = if !rem.is_empty() {
                            Some(rem)
                        } else {
                            step = 2;
                            args.get(i + 1).map(String::as_str)
                        };
                        if method.is_none_or(|m| !is_read_method(m)) {
                            return true;
                        }
                        break;
                    }
                    // Common curl valued short options so their value isn't scanned as flags.
                    'H' | 'u' | 'o' | 'A' | 'e' | 'b' | 'c' | 'w' | 'K' | 'm' | 'x' | 'U' => {
                        if rem.is_empty() {
                            step = 2;
                        }
                        break;
                    }
                    _ => {}
                }
            }
            i += step;
            continue;
        }
        i += 1;
    }
    false
}

fn wget_is_write(args: &[String]) -> bool {
    let mut i = 0;
    while let Some(arg) = args.get(i).map(String::as_str) {
        if arg == "--" {
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, glued) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            match name {
                "post-data" | "post-file" | "body-data" | "body-file" => return true,
                "method" => {
                    let method = glued.or_else(|| args.get(i + 1).map(String::as_str));
                    if method.is_none_or(|m| !is_read_method(m)) {
                        return true;
                    }
                    i += if glued.is_some() { 1 } else { 2 };
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }
    false
}

fn lwp_is_write(args: &[String]) -> bool {
    let mut i = 0;
    while let Some(arg) = args.get(i).map(String::as_str) {
        if arg == "--" {
            break;
        }
        if arg == "-m" {
            let method = args.get(i + 1).map(String::as_str);
            if method.is_none_or(|m| !is_read_method(m)) {
                return true;
            }
            i += 2;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("-m")
            && !is_read_method(rest)
        {
            return true;
        }
        i += 1;
    }
    false
}

fn httpie_is_write(args: &[String]) -> bool {
    let mut first_pos = true;
    let mut i = 0;
    while let Some(arg) = args.get(i).map(String::as_str) {
        if arg == "--" {
            i += 1;
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, glued) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            if name == "method" {
                let method = glued.or_else(|| args.get(i + 1).map(String::as_str));
                if method.is_none_or(|m| !is_read_method(m)) {
                    return true;
                }
                i += if glued.is_some() { 1 } else { 2 };
                continue;
            }
            i += 1;
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            if arg == "-X" {
                let method = args.get(i + 1).map(String::as_str);
                if method.is_none_or(|m| !is_read_method(m)) {
                    return true;
                }
                i += 2;
                continue;
            }
            if let Some(rest) = arg.strip_prefix("-X")
                && !is_read_method(rest)
            {
                return true;
            }
            i += 1;
            continue;
        }
        if first_pos {
            first_pos = false;
            if is_http_write_method(arg) {
                return true;
            }
        } else if is_kv_item(arg) {
            return true;
        }
        i += 1;
    }
    args[i..].iter().any(|arg| is_kv_item(arg))
}

fn generic_http_is_write(args: &[String]) -> bool {
    let mut i = 0;
    while let Some(arg) = args.get(i).map(String::as_str) {
        if arg == "--" {
            break;
        }
        if arg == "--post-data" || arg.starts_with("--post-data=") {
            return true;
        }
        if arg == "--method" || arg == "-X" {
            let method = args.get(i + 1).map(String::as_str);
            if method.is_none_or(|m| !is_read_method(m)) {
                return true;
            }
            i += 2;
            continue;
        }
        if let Some(m) = arg
            .strip_prefix("--method=")
            .or_else(|| arg.strip_prefix("-X"))
        {
            if !is_read_method(m) {
                return true;
            }
            i += 1;
            continue;
        }
        if !arg.starts_with('-') && is_kv_item(arg) {
            return true;
        }
        i += 1;
    }
    false
}

fn is_http_write_method(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "POST" | "PUT" | "PATCH" | "DELETE"
    )
}

fn is_kv_item(arg: &str) -> bool {
    if arg.contains("://") {
        return false;
    }
    if let Some((key, _)) = arg.split_once(":=").or_else(|| arg.split_once('=')) {
        return !key.is_empty();
    }
    false
}
