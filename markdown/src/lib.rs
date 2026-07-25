//! Shared markdown → sanitized-HTML pipeline for Adelie's webview chat clients.
//!
//! Assistant replies are **untrusted input**. The reducer deliberately ships raw
//! Markdown and treats presentation as the client's job, so every client that
//! renders that text in an HTML engine (WebKitGTK on Linux, `WKWebView` on
//! macOS) is one careless `innerHTML` away from arbitrary script execution in a
//! window that also talks to the daemon.
//!
//! This crate owns the two independent layers that close that hole, so there is
//! exactly one security-reviewed copy rather than one per client:
//!
//! 1. [`markdown_to_html`] renders Markdown and then **sanitizes the rendered
//!    HTML** with [`ammonia`], stripping `<script>`, event handlers, and unsafe
//!    URL schemes while keeping the text around them.
//! 2. The page templates ([`chat_page::html_template`], [`bubble::document`])
//!    serve their inline script under a **SHA-256-pinned CSP `script-src`**, so
//!    the engine refuses to execute anything else — including markup that
//!    somehow survived layer 1.
//!
//! Why the crate is separate from the `client-ui-common` reducer: the reducer
//! must stay `wasm32`-clean, and the HTML parser this pulls in is native-only
//! territory. Consumers depend on this crate directly.

use pulldown_cmark::{Event, Options, Parser, html};

pub mod bubble;
pub mod chat_page;
mod csp;

/// Convert markdown text to HTML and sanitize the result.
///
/// Two reasons to sanitize after `pulldown_cmark` rather than before:
///
/// 1. Raw HTML embedded in markdown (`<script>...</script>`, `<img onerror=...>`,
///    `<a href="javascript:...">`) is emitted verbatim by `pulldown_cmark`'s
///    HTML renderer. Stripping `Event::Html` / `Event::InlineHtml` works for
///    block-form attacks but loses adjacent legitimate text when the attacker
///    puts both on one line (e.g. `<script>x</script>hello` — pulldown-cmark
///    treats the entire run as a single HTML block). [`ammonia`] parses the
///    rendered HTML and strips dangerous constructs while preserving text.
/// 2. `ammonia`'s default builder whitelists the exact tags markdown produces
///    (headings, lists, code, links with safe URL schemes, etc.) and removes
///    event handlers, `<script>`, `<style>`, `<iframe>`, `<form>`, and any
///    `href` / `src` whose scheme isn't in the safe allowlist.
///
/// Combined with the SHA-256-pinned CSP `script-src` in the page templates,
/// this gives two independent layers against hostile assistant output.
pub fn markdown_to_html(input: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(input, options).map(task_marker_as_text);
    let mut raw = String::new();
    html::push_html(&mut raw, parser);

    ammonia::clean(&raw)
}

/// Ballot-box glyphs for a task-list item's state.
///
/// `pulldown_cmark` renders a task marker as `<input type="checkbox">`, which
/// the sanitizer removes — correctly, since an input is a form control, not
/// text, and admitting form controls to keep it would widen the allowlist for
/// every reply. But dropping it loses the one thing a task list says: a done
/// item and a pending one become identical bullets.
///
/// Emitting the state as *text* before sanitization keeps the meaning and needs
/// no allowlist change. The glyphs are ordinary characters, so a reply that
/// merely contains one is unaffected — they only appear here for a real marker.
const UNCHECKED_MARKER: &str = "\u{2610} "; // ☐
const CHECKED_MARKER: &str = "\u{2611} "; // ☑

/// Replace a task-list marker event with its text equivalent.
fn task_marker_as_text(event: Event<'_>) -> Event<'_> {
    match event {
        Event::TaskListMarker(true) => Event::Text(CHECKED_MARKER.into()),
        Event::TaskListMarker(false) => Event::Text(UNCHECKED_MARKER.into()),
        other => other,
    }
}
