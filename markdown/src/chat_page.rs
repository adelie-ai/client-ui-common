//! The full-transcript chat page used by the GTK client's WebKitGTK view.

/// Full HTML page template with embedded CSS for a whole chat transcript.
///
/// `script-src` is locked to the SHA-256 hash of the inline script body — no
/// `'unsafe-inline'`, no `'unsafe-eval'`, no remote scripts. Combined with the
/// sanitization in [`crate::markdown_to_html`], a hostile assistant message
/// cannot execute JavaScript in the chat WebView.
pub fn html_template() -> &'static str {
    todo!("lift from gtk-client::markdown")
}
