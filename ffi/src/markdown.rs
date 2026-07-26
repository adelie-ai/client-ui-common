//! C ABI for the shared markdown → sanitized-HTML pipeline.
//!
//! Every other entry point in this crate queues an intent and returns
//! immediately. These are the exception: rendering is pure, cheap, and the
//! caller needs the resulting string in hand to hand to its webview, so they are
//! synchronous and return a **caller-owned** C string.
//!
//! # Why this lives in the core at all
//!
//! Assistant replies are untrusted (gtk#25). The sanitizer + CSP-pinned page
//! pair in `adele-markdown` is the security boundary that keeps hostile markup
//! inert in an HTML engine. A client that reimplements it gets a second
//! implementation to audit and a guarantee of drift; a client that skips it gets
//! script execution in a window that also holds a daemon session. So the
//! boundary is exported here and every webview client calls the same code.
//!
//! # Ownership
//!
//! `adele_core_render_markdown`, `adele_core_render_markdown_document` and
//! `adele_core_markdown_set_content_script` return a NUL-terminated string
//! allocated by this library. Release it with [`adele_core_string_free`] —
//! never the caller's `free`, since the allocators need not match. The
//! bridge-name accessors return `'static` pointers that must **not** be freed.

use std::ffi::{CString, c_char};
use std::sync::OnceLock;

use adele_markdown::bubble;

use crate::cstr_to_string;

/// Move an owned `String` out to C as a caller-owned NUL-terminated buffer.
///
/// Returns an empty string rather than null when the value contains an interior
/// NUL. A caller checking only for null would otherwise be handed a truncated
/// document; an empty bubble is the safer failure.
fn into_c_string(value: String) -> *mut c_char {
    CString::new(value).unwrap_or_default().into_raw()
}

/// Render untrusted markdown into a **sanitized HTML fragment**, for a host
/// that splices markup into a page it builds itself.
///
/// The fragment is inert markup, not script. Do **not** format it into
/// JavaScript source — a rendered fragment carries raw double quotes and raw
/// newlines, so interpolating one into a call ends the string literal and
/// executes whatever the reply put after it, outside the page's pinned
/// `script-src`. To push content into a bubble page, call
/// [`adele_core_markdown_set_content_script`], which returns the whole
/// statement with the escaping already done.
///
/// Returns a caller-owned string to release with [`adele_core_string_free`];
/// null input renders as the empty string. Never returns null.
///
/// # Safety
/// `text` must be null or point to a valid NUL-terminated C string that stays
/// valid for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_render_markdown(text: *const c_char) -> *mut c_char {
    // SAFETY: contract above.
    let input = unsafe { cstr_to_string(text) };
    into_c_string(adele_markdown::markdown_to_html(&input))
}

/// Render untrusted markdown into a **complete, CSP-locked page** for a single
/// message bubble: transparent background, system-appearance aware, no network,
/// self-reporting height, and an in-place update hook.
///
/// This is what a host loads once per message (with a null base URL);
/// subsequent updates go through [`adele_core_markdown_set_content_script`],
/// which keeps the pinned script hash — and therefore the page — unchanged.
///
/// Returns a caller-owned string to release with [`adele_core_string_free`];
/// null input renders an empty bubble. Never returns null.
///
/// # Safety
/// `text` must be null or point to a valid NUL-terminated C string that stays
/// valid for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_render_markdown_document(text: *const c_char) -> *mut c_char {
    // SAFETY: contract above.
    let input = unsafe { cstr_to_string(text) };
    into_c_string(bubble::document(&input))
}

/// Name of the script-message handler the bubble page posts its pixel height
/// to. The host must register a handler under exactly this name; an embedded
/// engine does not self-size inside a native stack view.
///
/// Returns a `'static` pointer — do not free it.
#[unsafe(no_mangle)]
pub extern "C" fn adele_core_markdown_height_handler_name() -> *const c_char {
    static NAME: OnceLock<CString> = OnceLock::new();
    NAME.get_or_init(|| c_name(bubble::HEIGHT_MESSAGE_HANDLER))
        .as_ptr()
}

/// Render untrusted markdown into the **complete JavaScript statement** that
/// swaps it into an already-loaded bubble page — the streaming update that
/// follows [`adele_core_render_markdown_document`].
///
/// Evaluate the returned string verbatim (`WKWebView.evaluateJavaScript`). It
/// is one call to the page's update function with the reply rendered,
/// sanitized, and encoded as a JavaScript string literal, so the reply cannot
/// leave the literal and become code. Building that call from
/// [`adele_core_render_markdown`] and
/// [`adele_core_markdown_set_content_function`] instead is an injection: host
/// evaluation is exempt from the page's CSP, so nothing downstream would catch
/// it.
///
/// Returns a caller-owned string to release with [`adele_core_string_free`];
/// null input yields the statement that clears the bubble. Never returns null.
///
/// # Safety
/// `text` must be null or point to a valid NUL-terminated C string that stays
/// valid for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_markdown_set_content_script(
    text: *const c_char,
) -> *mut c_char {
    // SAFETY: contract above.
    let input = unsafe { cstr_to_string(text) };
    into_c_string(bubble::set_content_script(&input))
}

/// Name of the global function that swaps a new render into an already-loaded
/// bubble page.
///
/// For hosts that bind the page themselves — installing their own wrapper, or
/// asserting the bridge is present. It is **not** how to push content: use
/// [`adele_core_markdown_set_content_script`], which returns the whole call
/// already escaped.
///
/// Returns a `'static` pointer — do not free it.
#[unsafe(no_mangle)]
pub extern "C" fn adele_core_markdown_set_content_function() -> *const c_char {
    static NAME: OnceLock<CString> = OnceLock::new();
    NAME.get_or_init(|| c_name(bubble::SET_CONTENT_FUNCTION))
        .as_ptr()
}

/// Free a string returned by [`adele_core_render_markdown`],
/// [`adele_core_render_markdown_document`] or
/// [`adele_core_markdown_set_content_script`]. Null is a no-op.
///
/// # Safety
/// `text` must be null, or a pointer returned by one of those three functions
/// and not yet freed. Do not pass the `'static` pointers from the bridge-name
/// accessors.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_string_free(text: *mut c_char) {
    if text.is_null() {
        return;
    }
    // SAFETY: contract above — `text` came from `CString::into_raw`, so
    // reclaiming it as a `CString` restores the original allocation and drops it.
    drop(unsafe { CString::from_raw(text) });
}

/// NUL-terminate a compile-time bridge name so it can be handed out as a
/// `'static` C pointer.
fn c_name(name: &str) -> CString {
    CString::new(name).expect("bridge names are ASCII identifiers with no interior NUL")
}
