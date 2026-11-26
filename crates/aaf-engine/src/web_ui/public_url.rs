//! Strict HTTPS public-origin validation for reverse-proxied Cockpit access
//! (e.g. Tailscale Serve `https://<machine>.<tailnet>.ts.net`).

use std::fmt::Write as _;

use super::parse_authority;
use super::qr::render_qr_ascii;

/// Validated HTTPS public origin trusted as a reverse-proxy front-end for `aaf ui`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedOrigin {
    pub host: String,
    pub port: u16,
    /// Canonical origin without trailing slash (`https://host` or `https://host:port`).
    pub canonical: String,
}

impl TrustedOrigin {
    /// Returns `true` when an incoming HTTP `Host` header matches this trusted origin.
    pub fn matches_host(&self, host: &str, port: Option<u16>) -> bool {
        self.host.eq_ignore_ascii_case(host) && port.unwrap_or(443) == self.port
    }

    /// Returns `true` when an incoming HTTP `Origin` header matches this trusted HTTPS origin.
    pub fn matches_origin(&self, scheme_is_https: bool, host: &str, port: u16) -> bool {
        scheme_is_https && self.host.eq_ignore_ascii_case(host) && port == self.port
    }

    /// Canonical `Origin` header value (`https://host` or `https://host:port`).
    pub fn origin_header(&self) -> &str {
        &self.canonical
    }

    /// Clean HTTPS root URL (`https://host/`), safe for bookmarks and iOS Add-to-Home-Screen.
    pub fn clean_url(&self) -> String {
        format!("{}/", self.canonical)
    }

    /// Pairing URL carrying the bearer token solely in the URL fragment (`#token=...`).
    pub fn pairing_url(&self, token: &str) -> String {
        format!("{}/#token={token}", self.canonical)
    }

    /// Health dashboard URL carrying the bearer token solely in the URL fragment.
    pub fn health_url(&self, token: &str) -> String {
        format!("{}/health#token={token}", self.canonical)
    }

    /// Legacy alias for `health_url` (kept for backward compatibility).
    pub fn studio_url(&self, token: &str) -> String {
        self.health_url(token)
    }
}

/// Validates and normalizes a Tier-1 HTTPS public origin (fail-closed).
///
/// Only `https://<host>[:port]` (with optional single trailing `/`) is accepted.
/// Rejects `http://`, userinfo (`@`), paths, queries, fragments, wildcards, and control chars.
pub fn parse_trusted_origin(raw: &str) -> Result<TrustedOrigin, String> {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix("https://") else {
        return Err(
            "invalid public URL: must start with 'https://' (e.g. https://mac.tailnet.ts.net)"
                .to_string(),
        );
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.is_empty()
        || authority.contains('/')
        || authority.contains('?')
        || authority.contains('#')
        || authority.contains('@')
        || authority.contains('*')
    {
        return Err(
            "invalid public URL: must contain only https://<host>[:port] without path, query, or userinfo"
                .to_string(),
        );
    }
    let is_bracketed_ipv6 = authority.starts_with('[');
    let (host, opt_port) = parse_authority(authority)
        .ok_or_else(|| "invalid public URL: malformed host or port".to_string())?;
    if is_bracketed_ipv6 {
        let addr = host
            .parse::<std::net::Ipv6Addr>()
            .map_err(|_| "invalid public URL: malformed IPv6 address".to_string())?;
        if addr.is_unspecified() {
            return Err("invalid public URL: unspecified IPv6 address rejected".to_string());
        }
    } else {
        if host.contains(':') {
            return Err(
                "invalid public URL: unbracketed ':' in host (wrap IPv6 in [...])".to_string(),
            );
        }
        if let Ok(v4) = host.parse::<std::net::Ipv4Addr>()
            && v4.is_unspecified()
        {
            return Err("invalid public URL: unspecified IPv4 address rejected".to_string());
        }
    }
    if host.starts_with('.') || host.ends_with('.') {
        return Err("invalid public URL: wildcard or malformed host rejected".to_string());
    }
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
    {
        return Err("invalid public URL: host contains unsupported characters".to_string());
    }
    let port = opt_port.unwrap_or(443);
    if port == 0 {
        return Err("invalid public URL: port must be 1..=65535".to_string());
    }
    let formatted_host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.clone()
    };
    let canonical = if port == 443 {
        format!("https://{formatted_host}")
    } else {
        format!("https://{formatted_host}:{port}")
    };
    Ok(TrustedOrigin {
        host,
        port,
        canonical,
    })
}

/// Renders a terminal remote-access card + QR code for a trusted HTTPS origin.
///
/// If the full `#token=` URL exceeds the 106-byte Version 1..=5 QR capacity,
/// falls back to encoding the clean HTTPS root URL (ideal for iOS Add to Home Screen)
/// and prints the bearer token separately for one-tap paste.
pub fn render_remote_pairing_card(
    origin: &TrustedOrigin,
    token: &str,
    read_only: bool,
    include_qr: bool,
) -> String {
    let mode = if read_only {
        "read-only"
    } else {
        "interactive (approve locked)"
    };
    let full_url = origin.pairing_url(token);
    let clean = origin.clean_url();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "┌──────────────────────────────────────────────────────────┐"
    );
    let _ = writeln!(out, "│ AAF Mobile PWA · Remote HTTPS ({mode})");
    let _ = writeln!(out, "│ Clean URL (iOS Add to Home Screen): {clean}");
    let _ = writeln!(out, "│ Direct URL (with #token): {full_url}");
    let _ = writeln!(
        out,
        "│ Tip: On iPhone, open Clean URL → Share → Add to Home Screen, then paste token once."
    );
    let _ = writeln!(
        out,
        "└──────────────────────────────────────────────────────────┘"
    );
    if include_qr {
        if let Some(qr) = render_qr_ascii(&full_url) {
            out.push('\n');
            out.push_str(&qr);
        } else if let Some(qr) = render_qr_ascii(&clean) {
            let _ = writeln!(
                out,
                "\n(Full URL > 106 bytes; QR encodes clean HTTPS URL — paste token once after opening)"
            );
            out.push_str(&qr);
        } else {
            let _ = writeln!(
                out,
                "(URL exceeds compact QR capacity of 106 bytes; open the URL above directly)"
            );
        }
    }
    out
}
