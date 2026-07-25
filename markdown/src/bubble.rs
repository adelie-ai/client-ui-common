//! A single-message HTML document for clients that host one engine per bubble.
//!
//! macOS renders the transcript with native SwiftUI chrome and drops a
//! `WKWebView` in for the message body only, so it needs a *fragment-sized*
//! document rather than the whole-transcript page in [`crate::chat_page`]:
//! transparent background, no page chrome, self-reporting height, and an
//! in-place content update hook so a streaming reply never reloads.

/// Name of the `WKScriptMessageHandler` the page posts its height to.
pub const HEIGHT_MESSAGE_HANDLER: &str = "adeleBubble";

/// Name of the global function the host calls to swap the rendered body in
/// place (via `evaluateJavaScript`) without reloading the document.
pub const SET_CONTENT_FUNCTION: &str = "adeleSetContent";

/// Render `markdown` and wrap it in a standalone, CSP-locked bubble document.
pub fn document(_markdown: &str) -> String {
    todo!("build the bubble document")
}
