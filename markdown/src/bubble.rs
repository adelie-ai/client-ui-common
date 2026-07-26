//! A single-message HTML document for clients that host one engine per bubble.
//!
//! macOS renders the transcript with native SwiftUI chrome and drops a
//! `WKWebView` in for the message body only, so it needs a *fragment-sized*
//! document rather than the whole-transcript page in [`crate::chat_page`]:
//! transparent background, no page chrome, self-reporting height, and an
//! in-place content update hook so a streaming reply never reloads.
//!
//! # Contract with the host
//!
//! - Load [`document`] once per message with a `nil` base URL.
//! - On every subsequent content change, evaluate the string
//!   [`set_content_script`] returns, **verbatim**. It is a complete statement
//!   with the reply already rendered, sanitized and encoded as a JavaScript
//!   string literal. The document's inline script body is byte-identical
//!   regardless of content, so the pinned CSP hash never changes and the page
//!   never has to reload.
//! - Subscribe to the [`HEIGHT_MESSAGE_HANDLER`] script-message handler and use
//!   the posted number as the view's height — an embedded engine does not
//!   self-size inside a native stack view.
//!
//! # Why the host evaluates a whole statement instead of building the call
//!
//! `script-src` pins one hash and forbids `'unsafe-inline'` / `'unsafe-eval'`,
//! so nothing *in the page* can introduce script — verified empirically: a
//! second inline `<script>` in the document does not run, and `eval()` from
//! page script raises `EvalError`. Host-side script evaluation
//! (`WKWebView.evaluateJavaScript`) is not page script and is exempt, which is
//! what lets the host stream content in without weakening the policy.
//!
//! That exemption cuts both ways. A host that formats
//! `adeleSetContent("<html>")` itself is *writing a program* around untrusted
//! text, and an ordinary reply is enough to escape it — a rendered fragment
//! carries raw double quotes and raw newlines. The result would run outside the
//! pinned `script-src` with the message handlers in reach, defeating both other
//! layers. So the escaping lives here, on the shared side of the boundary, and
//! the host is never handed a fragment plus a function name. See [`crate::js`].

use std::sync::OnceLock;

use crate::csp::sha256_source;
use crate::markdown_to_html;

/// Name of the script-message handler the page posts its pixel height to.
///
/// The host must register a handler under exactly this name; the page silently
/// skips reporting when it is absent, so an unregistered handler shows up as a
/// bubble stuck at its initial height rather than a JS exception.
pub const HEIGHT_MESSAGE_HANDLER: &str = "adeleBubble";

/// Name of the global function that swaps the rendered body in place without
/// reloading the document.
///
/// Exposed for hosts that bind the page themselves — installing their own
/// wrapper, or asserting the bridge is present. It is **not** the way to push
/// content: use [`set_content_script`], which returns the whole call with the
/// reply already escaped. Formatting the call from this name and a rendered
/// fragment is the injection this module's docs describe.
pub const SET_CONTENT_FUNCTION: &str = "adeleSetContent";

/// Inline JavaScript body for the bubble document.
///
/// Deliberately tiny: it exists only to (a) accept new sanitized HTML from the
/// host and (b) tell the host how tall the content is. It never fetches, never
/// evaluates strings, and never touches anything outside `#content`.
///
/// The `__PLACEHOLDER__` tokens are substituted from the public constants above
/// so the JS and the host-facing API cannot drift apart.
const INLINE_SCRIPT_TEMPLATE: &str = r#"
(function () {
    var content = document.getElementById('content');
    var lastHeight = -1;

    function reportHeight() {
        var h = document.body.scrollHeight;
        if (h === lastHeight) {
            return;
        }
        lastHeight = h;
        if (window.webkit && window.webkit.messageHandlers
            && window.webkit.messageHandlers.__HEIGHT_HANDLER__) {
            window.webkit.messageHandlers.__HEIGHT_HANDLER__.postMessage(h);
        }
    }

    // Swap in a new render of the (possibly still streaming) message. The host
    // has already sanitized this HTML; the page never builds markup itself.
    window.__SET_CONTENT__ = function (html) {
        content.innerHTML = html;
        reportHeight();
    };

    // Height changes for reasons other than a content swap too: the host
    // resizing the view reflows text, and late layout settles the last line.
    if (window.ResizeObserver) {
        new ResizeObserver(reportHeight).observe(document.body);
    }
    window.addEventListener('load', reportHeight);
    reportHeight();
})();
"#;

/// Page shell. `__CONTENT__` is substituted last, so a message body can never
/// impersonate one of the earlier placeholders.
///
/// The CSP is stricter than the transcript page's: `img-src data:` only. A
/// bubble must not become a tracking pixel or a network-reachability oracle for
/// whoever authored the reply, so remote images do not load and fall back to
/// their alt text. `default-src 'none'` covers everything else — no fonts, no
/// stylesheets, no `fetch`, no frames.
const DOCUMENT_TEMPLATE: &str = r##"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data:; script-src __CSP_SCRIPT_HASH__;">
<style>
/* Let the engine pick native form/scrollbar rendering for the host's current
   appearance; the palette below follows it. */
:root {
    color-scheme: light dark;

    --fg: rgba(0, 0, 0, 0.87);
    --fg-dim: rgba(0, 0, 0, 0.55);
    --rule: rgba(0, 0, 0, 0.16);
    --surface: rgba(0, 0, 0, 0.06);
    --link: #0053c7;
}

@media (prefers-color-scheme: dark) {
    :root {
        --fg: rgba(255, 255, 255, 0.92);
        --fg-dim: rgba(255, 255, 255, 0.58);
        --rule: rgba(255, 255, 255, 0.20);
        --surface: rgba(255, 255, 255, 0.10);
        --link: #6fb0ff;
    }
}

* { margin: 0; padding: 0; box-sizing: border-box; }

/* The native bubble draws the background and rounds the corners; a painted
   page would show as a hard rectangle inside it. */
html, body {
    background: transparent;
    margin: 0;
    padding: 0;
}

body {
    color: var(--fg);
    font-family: -apple-system, BlinkMacSystemFont, system-ui, sans-serif;
    font-size: 13px;
    line-height: 1.5;
    overflow-wrap: anywhere;
    -webkit-text-size-adjust: none;
}

/* No leading/trailing margin: the native bubble owns the outer padding, and a
   stray collapsed margin would be baked into the reported height. */
#content > *:first-child { margin-top: 0; }
#content > *:last-child { margin-bottom: 0; }

p { margin: 0.5em 0; }

h1, h2, h3, h4, h5, h6 {
    margin: 0.8em 0 0.35em;
    line-height: 1.3;
    font-weight: 600;
}
h1 { font-size: 1.5em; }
h2 { font-size: 1.3em; }
h3 { font-size: 1.15em; }
h4, h5, h6 { font-size: 1em; }

ul, ol { margin: 0.5em 0; padding-left: 1.5em; }
li { margin: 0.15em 0; }
li > ul, li > ol { margin: 0.15em 0; }

blockquote {
    margin: 0.5em 0;
    padding: 0.1em 0 0.1em 0.8em;
    border-left: 3px solid var(--rule);
    color: var(--fg-dim);
}

pre {
    margin: 0.6em 0;
    padding: 10px 12px;
    background: var(--surface);
    border-radius: 6px;
    overflow-x: auto;
}

code {
    font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, monospace;
    font-size: 0.92em;
}

:not(pre) > code {
    padding: 0.12em 0.35em;
    background: var(--surface);
    border-radius: 4px;
}

/* `display: block` is what lets a wide table scroll inside the bubble instead
   of forcing the whole page wider than the host's frame. */
table {
    display: block;
    max-width: 100%;
    overflow-x: auto;
    border-collapse: collapse;
    margin: 0.6em 0;
}

th, td {
    border: 1px solid var(--rule);
    padding: 5px 10px;
    text-align: left;
}

th { background: var(--surface); font-weight: 600; }

a { color: var(--link); text-decoration: none; }
a:hover { text-decoration: underline; }

img { max-width: 100%; height: auto; }

hr { border: none; border-top: 1px solid var(--rule); margin: 0.9em 0; }

del { color: var(--fg-dim); }

/* A *loose* list — items separated by blank lines — has each item's content
   wrapped in a <p>, which would otherwise inherit the full paragraph margin and
   space the items apart like paragraphs. */
ul li p { margin: 0.15em 0; }
</style>
</head>
<body>
<div id="content">__CONTENT__</div>
<script>__INLINE_SCRIPT__</script>
</body>
</html>"##;

/// The inline script body, with the host-facing names substituted in.
fn inline_script() -> &'static str {
    static SCRIPT: OnceLock<String> = OnceLock::new();
    SCRIPT.get_or_init(|| {
        INLINE_SCRIPT_TEMPLATE
            .replace("__HEIGHT_HANDLER__", HEIGHT_MESSAGE_HANDLER)
            .replace("__SET_CONTENT__", SET_CONTENT_FUNCTION)
    })
}

/// The document shell, split around the point the message body is spliced in.
///
/// Precomputed so rendering a bubble is one hash-free string concatenation —
/// this runs once per streaming chunk per visible message.
fn shell() -> &'static (String, String) {
    static SHELL: OnceLock<(String, String)> = OnceLock::new();
    SHELL.get_or_init(|| {
        let script = inline_script();
        let page = DOCUMENT_TEMPLATE
            .replace("__CSP_SCRIPT_HASH__", &sha256_source(script))
            .replace("__INLINE_SCRIPT__", script);
        let split = page
            .find("__CONTENT__")
            .expect("the bubble document template contains the content placeholder");
        (
            page[..split].to_string(),
            page[split + "__CONTENT__".len()..].to_string(),
        )
    })
}

/// Render `markdown` and wrap it in a standalone, CSP-locked bubble document.
///
/// The markdown is untrusted: it goes through [`markdown_to_html`] here, so a
/// host cannot accidentally splice raw assistant text into the page.
pub fn document(markdown: &str) -> String {
    let (head, tail) = shell();
    let body = markdown_to_html(markdown);
    let mut out = String::with_capacity(head.len() + body.len() + tail.len());
    out.push_str(head);
    out.push_str(&body);
    out.push_str(tail);
    out
}

/// Build the complete JavaScript statement that replaces a loaded bubble's body
/// with a new render of `markdown` — what a host evaluates on every streaming
/// update after the initial [`document`] load.
///
/// Takes the same untrusted markdown [`document`] does, and returns something
/// that is already script: the reply is rendered, sanitized, and encoded as a
/// JavaScript string literal ([`crate::js::string_literal`]). A host evaluates
/// the returned string verbatim and never sees, or has to quote, the fragment
/// in between — which is the point. There is no supported path where a host
/// assembles this call itself.
///
/// ```
/// use adele_markdown::bubble;
///
/// // A reply that tries to close the call stays inside the literal: one
/// // statement, one argument, and no raw newline to end the line early.
/// let script = bubble::set_content_script(r#"He said "run me");alert(1);//"#);
/// assert!(script.starts_with("adeleSetContent(\""));
/// assert!(script.ends_with("\");"));
/// assert!(!script.contains('\n'));
/// ```
pub fn set_content_script(markdown: &str) -> String {
    format!(
        "{SET_CONTENT_FUNCTION}({});",
        crate::js::string_literal(&markdown_to_html(markdown))
    )
}
