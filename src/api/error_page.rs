// SPDX-License-Identifier: BUSL-1.1

//! The one themed HTML page for errors a browser lands on outside the SPA
//! (OAuth callback failures, an unknown `/_/` page). It follows the UI
//! theme: the `dg-theme` choice in localStorage, else the OS preference.

/// Escape text for HTML element content and attribute values.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// A complete HTML document: `title` as the heading, `message` below it,
/// and a link back to the UI. Both strings are escaped here.
pub fn themed_error_html(title: &str, message: &str) -> String {
    let title = escape_html(title);
    let message = escape_html(message);
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
  :root {{ --bg:#080c14; --card:#111827; --border:#1f2937; --text:#e2e8f0; --muted:#9ca3af;
          --error:#f87171; --accent:#2dd4bf; --accent-hover:#14b8a6; --accent-text:#080c14; }}
  :root.light {{ --bg:#f5f7fa; --card:#ffffff; --border:#e2e8f0; --text:#1e293b; --muted:#64748b;
                 --error:#e11d48; --accent:#0d9488; --accent-hover:#0f766e; --accent-text:#ffffff; }}
  body {{ font-family: 'Outfit',system-ui,-apple-system,sans-serif; background:var(--bg); color:var(--text);
         display:flex; align-items:center; justify-content:center; min-height:100vh; margin:0; }}
  .card {{ background:var(--card); border:1px solid var(--border); border-radius:12px; padding:40px;
           max-width:420px; text-align:center; }}
  h1 {{ font-size:20px; margin:0 0 12px; color:var(--error); }}
  p {{ font-size:14px; color:var(--muted); line-height:1.6; margin:0 0 24px; overflow-wrap:anywhere; }}
  a {{ display:inline-block; padding:10px 24px; background:var(--accent); color:var(--accent-text);
       border-radius:8px; text-decoration:none; font-weight:600; font-size:14px; outline-offset:3px; }}
  a:hover {{ background:var(--accent-hover); }}
  a:focus-visible {{ outline:2px solid var(--accent); }}
</style>
<script>try{{var t=localStorage.getItem('dg-theme');if(t==='light'||(!t&&matchMedia('(prefers-color-scheme:light)').matches))document.documentElement.classList.add('light')}}catch(e){{}}</script>
</head>
<body>
  <main class="card">
    <h1 role="alert">{title}</h1>
    <p>{message}</p>
    <a href="/_/" aria-label="Return to DeltaGlider Proxy">Back to Home</a>
  </main>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_escapes_and_follows_the_ui_theme() {
        let html = themed_error_html("Page not found", "No page at /_/<script>");
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("/_/<script>"));
        assert!(html.contains("localStorage.getItem('dg-theme')"));
        assert!(html.contains("prefers-color-scheme:light"));
    }
}
