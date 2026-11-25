//! Embedded static PWA metadata and app icons (`manifest.webmanifest`,
//! `apple-touch-icon.png`, and `icon.svg`) served with zero external CDN dependencies.

use std::io::Write;
use std::net::TcpStream;

/// Crisp 180x180 PNG icon required by iOS Safari for `Add to Home Screen`.
pub const APPLE_TOUCH_ICON_PNG: &[u8] = include_bytes!("apple-touch-icon.png");

/// Embedded zero-CDN interactive structured diff & inline review JS module.
pub const DIFF_UI_JS: &str = include_str!("diff_ui.js");

/// Embedded zero-CDN SVG pipeline DAG, SOP runs, and Studio score JS module.
pub const PIPELINE_UI_JS: &str = include_str!("pipeline_ui.js");

/// Vector app icon matching the 3D hexagonal core emblem.
pub const ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 180 180">
  <defs>
    <linearGradient id="bg" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" stop-color="#090F1F"/>
      <stop offset="100%" stop-color="#1E3A8A"/>
    </linearGradient>
    <linearGradient id="hex" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" stop-color="#E2E8F0"/>
      <stop offset="50%" stop-color="#64748B"/>
      <stop offset="100%" stop-color="#6366F1"/>
    </linearGradient>
    <radialGradient id="core" cx="45%" cy="40%" r="55%">
      <stop offset="0%" stop-color="#A7F3D0"/>
      <stop offset="55%" stop-color="#38BDF8"/>
      <stop offset="100%" stop-color="#0284C7"/>
    </radialGradient>
  </defs>
  <rect width="180" height="180" rx="38" fill="url(#bg)"/>
  <polygon points="90,26 145,58 145,122 90,154 35,122 35,58" fill="none" stroke="url(#hex)" stroke-width="14" stroke-linejoin="round"/>
  <ellipse cx="90" cy="90" rx="42" ry="18" transform="rotate(-32 90 90)" fill="none" stroke="#CBD5E1" stroke-width="6" stroke-opacity="0.85"/>
  <polygon points="90,66 111,78 111,102 90,114 69,102 69,78" fill="url(#core)"/>
</svg>
"##;

/// Web App Manifest enabling full-screen standalone launch on iOS and Android.
pub const MANIFEST_JSON: &str = r##"{
  "name": "AutoAgentFactory Dashboard",
  "short_name": "AAF Dashboard",
  "description": "AutoAgentFactory Kanban Cockpit & Studio",
  "start_url": "/",
  "scope": "/",
  "display": "standalone",
  "background_color": "#0B0E14",
  "theme_color": "#0F172A",
  "icons": [
    {
      "src": "/apple-touch-icon.png",
      "sizes": "180x180",
      "type": "image/png",
      "purpose": "any"
    },
    {
      "src": "/icon.svg",
      "sizes": "any",
      "type": "image/svg+xml",
      "purpose": "any"
    }
  ]
}
"##;

/// Writes a static PWA asset response with strict security headers and safe caching.
pub(super) fn write_pwa_asset(
    stream: &mut TcpStream,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         X-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\n\
         Referrer-Policy: no-referrer\r\nCache-Control: private, max-age=3600\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Dispatches static PWA & UI asset routes (`0`=manifest, `1`=png, `2`=svg, `3`=diff.js, `4`=pipeline.js).
pub(super) fn serve_static(stream: &mut TcpStream, kind: u8) {
    let _ = match kind {
        0 => write_pwa_asset(
            stream,
            "application/manifest+json; charset=utf-8",
            MANIFEST_JSON.as_bytes(),
        ),
        1 => write_pwa_asset(stream, "image/png", APPLE_TOUCH_ICON_PNG),
        2 => write_pwa_asset(stream, "image/svg+xml; charset=utf-8", ICON_SVG.as_bytes()),
        3 => write_pwa_asset(
            stream,
            "application/javascript; charset=utf-8",
            DIFF_UI_JS.as_bytes(),
        ),
        _ => write_pwa_asset(
            stream,
            "application/javascript; charset=utf-8",
            PIPELINE_UI_JS.as_bytes(),
        ),
    };
}
