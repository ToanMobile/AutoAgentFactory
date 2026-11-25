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

/// Embedded zero-CDN bilingual EN / VI i18n JS module.
pub const I18N_UI_JS: &str = include_str!("i18n_ui.js");

/// Tactile 3D ceramic Agent Bot Pod icon (white chassis + glossy black visor + cyan pill eyes) legible down to 16px.
pub const ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 180 180">
  <defs>
    <linearGradient id="bg" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" stop-color="#F5F7FA"/>
      <stop offset="100%" stop-color="#D7DFE9"/>
    </linearGradient>
    <linearGradient id="body" x1="20%" y1="5%" x2="80%" y2="95%">
      <stop offset="0%" stop-color="#FFFFFF"/>
      <stop offset="62%" stop-color="#F1F4F8"/>
      <stop offset="100%" stop-color="#CBD4E1"/>
    </linearGradient>
    <linearGradient id="visor" x1="50%" y1="0%" x2="50%" y2="100%">
      <stop offset="0%" stop-color="#1E2430"/>
      <stop offset="28%" stop-color="#05070B"/>
      <stop offset="100%" stop-color="#000000"/>
    </linearGradient>
    <linearGradient id="gloss" x1="50%" y1="0%" x2="50%" y2="100%">
      <stop offset="0%" stop-color="#FFFFFF" stop-opacity="0.45"/>
      <stop offset="100%" stop-color="#FFFFFF" stop-opacity="0"/>
    </linearGradient>
    <linearGradient id="eye" x1="50%" y1="0%" x2="50%" y2="100%">
      <stop offset="0%" stop-color="#5CEEFF"/>
      <stop offset="55%" stop-color="#00D4FF"/>
      <stop offset="100%" stop-color="#00A8E8"/>
    </linearGradient>
  </defs>
  <rect width="180" height="180" rx="40" fill="url(#bg)"/>
  <rect x="24" y="29" width="132" height="128" rx="40" fill="#64748B" opacity="0.22"/>
  <rect x="24" y="24" width="132" height="132" rx="40" fill="url(#body)"/>
  <rect x="44" y="45" width="92" height="90" rx="28" fill="url(#visor)"/>
  <path d="M56 54 Q90 46 124 54 Q120 66 90 66 Q60 66 56 54 Z" fill="url(#gloss)"/>
  <rect x="66" y="73" width="14" height="34" rx="7" fill="#00D4FF" opacity="0.35" transform="scale(1.12)" transform-origin="73 90"/>
  <rect x="100" y="73" width="14" height="34" rx="7" fill="#00D4FF" opacity="0.35" transform="scale(1.12)" transform-origin="107 90"/>
  <rect x="67" y="74" width="12" height="32" rx="6" fill="url(#eye)"/>
  <rect x="101" y="74" width="12" height="32" rx="6" fill="url(#eye)"/>
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
      "src": "/apple-touch-icon.png?v=4",
      "sizes": "180x180",
      "type": "image/png",
      "purpose": "any"
    },
    {
      "src": "/icon.svg?v=4",
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
         Referrer-Policy: no-referrer\r\nCache-Control: no-cache\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Static PWA & embedded UI asset variants served without auth.
#[derive(Clone, Copy)]
pub(super) enum StaticAsset {
    Manifest,
    AppleTouchIcon,
    IconSvg,
    DiffJs,
    PipelineJs,
    I18nJs,
}

/// Dispatches static PWA & UI asset routes.
pub(super) fn serve_static(stream: &mut TcpStream, asset: StaticAsset) {
    let _ = match asset {
        StaticAsset::Manifest => write_pwa_asset(
            stream,
            "application/manifest+json; charset=utf-8",
            MANIFEST_JSON.as_bytes(),
        ),
        StaticAsset::AppleTouchIcon => write_pwa_asset(stream, "image/png", APPLE_TOUCH_ICON_PNG),
        StaticAsset::IconSvg => {
            write_pwa_asset(stream, "image/svg+xml; charset=utf-8", ICON_SVG.as_bytes())
        }
        StaticAsset::DiffJs => write_pwa_asset(
            stream,
            "application/javascript; charset=utf-8",
            DIFF_UI_JS.as_bytes(),
        ),
        StaticAsset::PipelineJs => write_pwa_asset(
            stream,
            "application/javascript; charset=utf-8",
            PIPELINE_UI_JS.as_bytes(),
        ),
        StaticAsset::I18nJs => write_pwa_asset(
            stream,
            "application/javascript; charset=utf-8",
            I18N_UI_JS.as_bytes(),
        ),
    };
}
