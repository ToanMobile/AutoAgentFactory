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

/// Vector app icon for desktop/Android favicons and PWA splash.
pub const ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 180 180">
  <defs>
    <radialGradient id="bg" cx="25%" cy="20%" r="85%">
      <stop offset="0%" stop-color="#2563EB"/>
      <stop offset="100%" stop-color="#0F172A"/>
    </radialGradient>
  </defs>
  <rect width="180" height="180" rx="38" fill="url(#bg)"/>
  <rect x="16" y="16" width="148" height="148" rx="28" fill="none" stroke="#60A5FA" stroke-opacity="0.35" stroke-width="2.5"/>
  <path d="M90 42 L128 136 L111 136 L102 112 L78 112 L69 136 L52 136 Z M83 98 L97 98 L90 74 Z" fill="#F8FAFC"/>
  <circle cx="138" cy="44" r="11" fill="#34D399"/>
</svg>
"##;

/// Web App Manifest enabling full-screen standalone launch on iOS and Android.
pub const MANIFEST_JSON: &str = r##"{
  "name": "AAF Cockpit",
  "short_name": "AAF Cockpit",
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
