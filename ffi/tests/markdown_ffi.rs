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
    adele_core_render_markdown, adele_core_render_markdown_document, adele_core_string_free,
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
        "<script>alert(1)</script>ok <img src=x onerror=alert(2)> [q](javascript:alert(3))",
    );
    let lower = html.to_ascii_lowercase();
    for bad in ["<script", "onerror", "javascript:", "alert("] {
        assert!(!lower.contains(bad), "{bad} survived: {html}");
    }
    assert!(html.contains("ok"), "{html}");
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
