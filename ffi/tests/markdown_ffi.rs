//! Spec for the C-ABI markdown surface.
//!
//! adele-mac links this cdylib and has no Rust of its own, so the shared
//! markdown → sanitize → CSP-locked-page pipeline has to be reachable over the
//! C ABI. These functions are synchronous (unlike every other `adele_core_*`
//! entry point, which queues an intent) because rendering is pure and the
//! caller needs the string to hand straight to its webview.

use std::ffi::{CStr, CString};

use adele_client_core::{
    adele_core_markdown_height_handler_name, adele_core_markdown_set_content_function,
    adele_core_markdown_set_content_script, adele_core_render_markdown,
    adele_core_render_markdown_document, adele_core_string_free,
};

/// Call a `*const c_char -> *mut c_char` entry point and take ownership of the
/// result as a Rust `String`, freeing it through the library's own allocator.
fn render(
    f: unsafe extern "C" fn(*const std::ffi::c_char) -> *mut std::ffi::c_char,
    input: &str,
) -> String {
    let c = CString::new(input).expect("test input has no interior NUL");
    // SAFETY: `c` outlives the call; the returned pointer is owned by us and
    // freed below through the library's matching free.
    unsafe {
        let out = f(c.as_ptr());
        assert!(
            !out.is_null(),
            "render must not return null for valid input"
        );
        let s = CStr::from_ptr(out).to_string_lossy().into_owned();
        adele_core_string_free(out);
        s
    }
}

#[test]
fn render_markdown_returns_a_sanitized_html_fragment() {
    let html = render(adele_core_render_markdown, "# Hi\n\n**bold**");
    assert!(html.contains("<h1>Hi</h1>"), "{html}");
    assert!(html.contains("<strong>bold</strong>"), "{html}");
    // A fragment, not a document — the host splices this into a live page.
    assert!(!html.contains("<!DOCTYPE"), "{html}");
}

#[test]
fn render_markdown_neutralizes_hostile_input() {
    let html = render(
        adele_core_render_markdown,
        "<script>alert(1)</script>ok <img src=x onerror=alert(2)>\n\n[q](javascript:alert(3))",
    );
    let lower = html.to_ascii_lowercase();
    for bad in [
        "<script",
        "onerror",
        "alert(1)",
        "alert(2)",
        "href=\"javascript:",
    ] {
        assert!(!lower.contains(bad), "{bad} survived: {html}");
    }
    assert!(html.contains("ok"), "{html}");
}

#[test]
fn render_markdown_strips_the_href_from_a_javascript_link() {
    // What "neutralized" means for a link: the anchor may survive as inert
    // markup, but it must carry no `href` at all — there is nothing to
    // navigate to, so nothing to execute. Asserting "the substring
    // `javascript:` never appears anywhere" would be asserting the wrong
    // thing: an unparsed link (one sharing a line with a raw-HTML *block*)
    // comes out as escaped literal text, which is inert by construction.
    let html = render(
        adele_core_render_markdown,
        "<b>x</b> [q](javascript:alert(3))",
    );
    assert!(html.contains("<a "), "the anchor itself may remain: {html}");
    assert!(!html.contains("href"), "but with no href: {html}");
    assert!(html.contains(">q</a>"), "link text survives: {html}");
}

#[test]
fn render_markdown_document_returns_a_csp_locked_page() {
    let doc = render(adele_core_render_markdown_document, "| a |\n|---|\n| 1 |");
    assert!(doc.starts_with("<!DOCTYPE html>"), "{doc}");
    assert!(doc.contains("<table>"), "{doc}");
    assert!(doc.contains("default-src 'none'"), "{doc}");
    assert!(doc.contains("script-src 'sha256-"), "{doc}");
}

#[test]
fn render_markdown_treats_null_as_empty() {
    // SAFETY: null is an explicitly supported argument.
    unsafe {
        let out = adele_core_render_markdown(std::ptr::null());
        assert!(
            !out.is_null(),
            "null input must still yield an owned string"
        );
        assert_eq!(CStr::from_ptr(out).to_bytes(), b"");
        adele_core_string_free(out);
    }
}

#[test]
fn string_free_tolerates_null() {
    // SAFETY: freeing null is a documented no-op.
    unsafe { adele_core_string_free(std::ptr::null_mut()) };
}

#[test]
fn set_content_script_is_exported_as_a_complete_already_escaped_statement() {
    // The host that most needs this has no Rust of its own, so the C ABI must
    // hand it something it can evaluate verbatim — not a function name plus the
    // job of escaping an untrusted fragment into a JS string literal.
    let reply = r#"He said "x");alert(document.cookie);//"#;
    let script = render(adele_core_markdown_set_content_script, reply);

    assert_eq!(
        script,
        adele_markdown::bubble::set_content_script(reply),
        "the C ABI must emit exactly what the shared crate does"
    );
    assert!(
        script.starts_with(&format!(
            "{}(",
            adele_markdown::bubble::SET_CONTENT_FUNCTION
        )),
        "{script}"
    );
    assert!(script.ends_with(");"), "{script}");
    assert!(
        !script.contains("\");alert("),
        "the reply broke out of the literal: {script}"
    );
    assert!(!script.contains('\n'), "no raw newline: {script}");
}

#[test]
fn set_content_script_neutralizes_hostile_markup_before_escaping_it() {
    let script = render(
        adele_core_markdown_set_content_script,
        "<script>alert(1)</script>ok <img src=x onerror=alert(2)>",
    );
    let lower = script.to_ascii_lowercase();
    for bad in ["<script", "onerror", "alert(1)", "alert(2)"] {
        assert!(!lower.contains(bad), "{bad} survived: {script}");
    }
    assert!(script.contains("ok"), "{script}");
}

#[test]
fn set_content_script_treats_null_as_an_empty_reply() {
    // SAFETY: null is an explicitly supported argument.
    unsafe {
        let out = adele_core_markdown_set_content_script(std::ptr::null());
        assert!(!out.is_null(), "null input must still yield a statement");
        let script = CStr::from_ptr(out).to_string_lossy().into_owned();
        adele_core_string_free(out);
        assert_eq!(
            script,
            format!("{}(\"\");", adele_markdown::bubble::SET_CONTENT_FUNCTION),
            "an empty reply clears the bubble rather than leaving stale content"
        );
    }
}

#[test]
fn bridge_names_are_exported_for_the_host_to_bind() {
    // SAFETY: both return pointers to `'static` NUL-terminated strings.
    let (handler, setter) = unsafe {
        (
            CStr::from_ptr(adele_core_markdown_height_handler_name())
                .to_str()
                .expect("ASCII"),
            CStr::from_ptr(adele_core_markdown_set_content_function())
                .to_str()
                .expect("ASCII"),
        )
    };
    assert_eq!(handler, adele_markdown::bubble::HEIGHT_MESSAGE_HANDLER);
    assert_eq!(setter, adele_markdown::bubble::SET_CONTENT_FUNCTION);

    // And the document actually wires those names up.
    let doc = render(adele_core_render_markdown_document, "hi");
    assert!(doc.contains(&format!("messageHandlers.{handler}")), "{doc}");
    assert!(doc.contains(setter), "{doc}");
}
