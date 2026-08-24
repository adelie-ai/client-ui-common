//! C ABI for the shared markdown -> sanitized-HTML pipeline.
//!
//! Every other entry point in this crate queues an intent and returns
//! immediately. These are the exception: rendering is pure, cheap, and the
//! caller needs the resulting string in hand to hand to its webview, so they are
//! synchronous and return a **caller-owned** C string.
//!
//! Every function here runs its body behind this crate's internal
//! `panic_guard::guard` helper, the same as every other entry point in this
//! crate. A caught panic never returns null from these six: the two
//! caller-owned-string renderers and the streaming-update statement return a
//! cached rendering of empty input, and the two bridge-name accessors return
//! the same `'static` pointer the success path would have produced.
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
//! allocated by this library. Release it with [`adele_core_string_free`] --
//! never the caller's `free`, since the allocators need not match. The
//! bridge-name accessors return `'static` pointers that must **not** be freed.

use std::ffi::{CString, c_char};
use std::sync::OnceLock;

use adele_markdown::bubble;

use crate::panic_guard;
use crate::{cstr_n_to_string, cstr_to_string};

/// Move an owned `String` out to C as a caller-owned NUL-terminated buffer.
///
/// Returns an empty string rather than null when the value contains an interior
/// NUL. A caller checking only for null would otherwise be handed a truncated
/// document; an empty bubble is the safer failure.
fn into_c_string(value: String) -> *mut c_char {
    CString::new(value).unwrap_or_default().into_raw()
}

/// A fresh, caller-owned copy of the cached rendering of `input`, computed
/// once via `render` and cloned on every call after the first.
///
/// Used both as the empty-input fast path these three functions already
/// documented, and as the panic fallback: the pipeline that rendered empty
/// input once is not rerun (and cannot re-panic) just because a *different*
/// call, with different input, panicked.
fn cached_owned_string(
    cache: &'static OnceLock<String>,
    render: impl FnOnce() -> String,
) -> *mut c_char {
    into_c_string(cache.get_or_init(render).clone())
}

/// Shared logic behind [`adele_core_render_markdown`] and
/// [`adele_core_render_markdown_n`]: run the already-decoded input through
/// the sanitized-fragment pipeline.
fn render_markdown_impl(input: String) -> *mut c_char {
    maybe_panic_for_test();
    into_c_string(adele_markdown::markdown_to_html(&input))
}

/// Render untrusted markdown into a **sanitized HTML fragment**, for a host
/// that splices markup into a page it builds itself.
///
/// The fragment is inert markup, not script. Do **not** format it into
/// JavaScript source -- a rendered fragment carries raw double quotes and raw
/// newlines, so interpolating one into a call ends the string literal and
/// executes whatever the reply put after it, outside the page's pinned
/// `script-src`. To push content into a bubble page, call
/// [`adele_core_markdown_set_content_script`], which returns the whole
/// statement with the escaping already done.
///
/// Returns a caller-owned string to release with [`adele_core_string_free`];
/// null input renders as the empty string. Never returns null: a caught panic
/// returns the cached rendering of empty input instead.
///
/// # Safety
/// `text` must be null or point to a valid NUL-terminated C string that stays
/// valid for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_render_markdown(text: *const c_char) -> *mut c_char {
    panic_guard::guard(
        "adele_core_render_markdown",
        move || {
            // SAFETY: contract above.
            let input = unsafe { cstr_to_string(text) };
            render_markdown_impl(input)
        },
        || {
            static EMPTY_FRAGMENT: OnceLock<String> = OnceLock::new();
            cached_owned_string(&EMPTY_FRAGMENT, || adele_markdown::markdown_to_html(""))
        },
    )
}

/// Length-carrying twin of [`adele_core_render_markdown`]. `text_len` is the
/// number of bytes at `text` -- see [`cstr_n_to_string`] for the exact decode
/// semantics (null/zero-length -> empty, embedded NUL kept, invalid UTF-8
/// replaced lossily).
///
/// Returns a caller-owned string to release with [`adele_core_string_free`].
/// Never returns null: a caught panic returns the cached rendering of empty
/// input instead.
///
/// # Safety
/// `text` must be null (with any length), or point to at least `text_len`
/// readable bytes for the duration of the call. A length longer than the
/// pointer's true allocation is the caller's error and is not detectable
/// here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_render_markdown_n(
    text: *const c_char,
    text_len: usize,
) -> *mut c_char {
    panic_guard::guard(
        "adele_core_render_markdown_n",
        move || {
            // SAFETY: contract above.
            let input = unsafe { cstr_n_to_string(text, text_len) };
            render_markdown_impl(input)
        },
        || {
            static EMPTY_FRAGMENT: OnceLock<String> = OnceLock::new();
            cached_owned_string(&EMPTY_FRAGMENT, || adele_markdown::markdown_to_html(""))
        },
    )
}

/// Shared logic behind [`adele_core_render_markdown_document`] and
/// [`adele_core_render_markdown_document_n`]: run the already-decoded input
/// through the bubble-document pipeline.
fn render_markdown_document_impl(input: String) -> *mut c_char {
    maybe_panic_for_test();
    into_c_string(bubble::document(&input))
}

/// Render untrusted markdown into a **complete, CSP-locked page** for a single
/// message bubble: transparent background, system-appearance aware, no network,
/// self-reporting height, and an in-place update hook.
///
/// This is what a host loads once per message (with a null base URL);
/// subsequent updates go through [`adele_core_markdown_set_content_script`],
/// which keeps the pinned script hash -- and therefore the page -- unchanged.
///
/// Returns a caller-owned string to release with [`adele_core_string_free`];
/// null input renders an empty bubble. Never returns null: a caught panic
/// returns the cached rendering of an empty bubble instead.
///
/// # Safety
/// `text` must be null or point to a valid NUL-terminated C string that stays
/// valid for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_render_markdown_document(text: *const c_char) -> *mut c_char {
    panic_guard::guard(
        "adele_core_render_markdown_document",
        move || {
            // SAFETY: contract above.
            let input = unsafe { cstr_to_string(text) };
            render_markdown_document_impl(input)
        },
        || {
            static EMPTY_DOCUMENT: OnceLock<String> = OnceLock::new();
            cached_owned_string(&EMPTY_DOCUMENT, || bubble::document(""))
        },
    )
}

/// Length-carrying twin of [`adele_core_render_markdown_document`].
/// `text_len` is the number of bytes at `text` -- see [`cstr_n_to_string`]
/// for the exact decode semantics (null/zero-length -> empty, embedded NUL
/// kept, invalid UTF-8 replaced lossily).
///
/// Returns a caller-owned string to release with [`adele_core_string_free`].
/// Never returns null: a caught panic returns the cached rendering of an
/// empty bubble instead.
///
/// # Safety
/// `text` must be null (with any length), or point to at least `text_len`
/// readable bytes for the duration of the call. A length longer than the
/// pointer's true allocation is the caller's error and is not detectable
/// here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_render_markdown_document_n(
    text: *const c_char,
    text_len: usize,
) -> *mut c_char {
    panic_guard::guard(
        "adele_core_render_markdown_document_n",
        move || {
            // SAFETY: contract above.
            let input = unsafe { cstr_n_to_string(text, text_len) };
            render_markdown_document_impl(input)
        },
        || {
            static EMPTY_DOCUMENT: OnceLock<String> = OnceLock::new();
            cached_owned_string(&EMPTY_DOCUMENT, || bubble::document(""))
        },
    )
}

/// Name of the script-message handler the bubble page posts its pixel height
/// to. The host must register a handler under exactly this name; an embedded
/// engine does not self-size inside a native stack view.
///
/// Returns a `'static` pointer -- do not free it. A caught panic returns the
/// same `'static` pointer the success path would have produced.
#[unsafe(no_mangle)]
pub extern "C" fn adele_core_markdown_height_handler_name() -> *const c_char {
    /// The `'static` fallback, spelled as a compile-time-checked C string
    /// literal so producing it can never itself panic. Kept in sync with
    /// [`bubble::HEIGHT_MESSAGE_HANDLER`] by the pipeline test below.
    static FALLBACK: &std::ffi::CStr = c"adeleBubble";
    panic_guard::guard(
        "adele_core_markdown_height_handler_name",
        || {
            static NAME: OnceLock<CString> = OnceLock::new();
            NAME.get_or_init(|| c_name(bubble::HEIGHT_MESSAGE_HANDLER))
                .as_ptr()
        },
        || FALLBACK.as_ptr(),
    )
}

/// Shared logic behind [`adele_core_markdown_set_content_script`] and
/// [`adele_core_markdown_set_content_script_n`]: run the already-decoded
/// input through the streaming-update statement pipeline.
fn markdown_set_content_script_impl(input: String) -> *mut c_char {
    maybe_panic_for_test();
    into_c_string(bubble::set_content_script(&input))
}

/// Render untrusted markdown into the **complete JavaScript statement** that
/// swaps it into an already-loaded bubble page -- the streaming update that
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
/// null input yields the statement that clears the bubble. Never returns null:
/// a caught panic returns that same clearing statement instead.
///
/// # Safety
/// `text` must be null or point to a valid NUL-terminated C string that stays
/// valid for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_markdown_set_content_script(
    text: *const c_char,
) -> *mut c_char {
    panic_guard::guard(
        "adele_core_markdown_set_content_script",
        move || {
            // SAFETY: contract above.
            let input = unsafe { cstr_to_string(text) };
            markdown_set_content_script_impl(input)
        },
        || {
            static CLEAR_SCRIPT: OnceLock<String> = OnceLock::new();
            cached_owned_string(&CLEAR_SCRIPT, || bubble::set_content_script(""))
        },
    )
}

/// Length-carrying twin of [`adele_core_markdown_set_content_script`].
/// `text_len` is the number of bytes at `text` -- see [`cstr_n_to_string`]
/// for the exact decode semantics (null/zero-length -> empty, embedded NUL
/// kept, invalid UTF-8 replaced lossily).
///
/// Returns a caller-owned string to release with [`adele_core_string_free`].
/// Never returns null: a caught panic returns the same clearing statement
/// instead.
///
/// # Safety
/// `text` must be null (with any length), or point to at least `text_len`
/// readable bytes for the duration of the call. A length longer than the
/// pointer's true allocation is the caller's error and is not detectable
/// here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_markdown_set_content_script_n(
    text: *const c_char,
    text_len: usize,
) -> *mut c_char {
    panic_guard::guard(
        "adele_core_markdown_set_content_script_n",
        move || {
            // SAFETY: contract above.
            let input = unsafe { cstr_n_to_string(text, text_len) };
            markdown_set_content_script_impl(input)
        },
        || {
            static CLEAR_SCRIPT: OnceLock<String> = OnceLock::new();
            cached_owned_string(&CLEAR_SCRIPT, || bubble::set_content_script(""))
        },
    )
}

/// Name of the global function that swaps a new render into an already-loaded
/// bubble page.
///
/// For hosts that bind the page themselves -- installing their own wrapper, or
/// asserting the bridge is present. It is **not** how to push content: use
/// [`adele_core_markdown_set_content_script`], which returns the whole call
/// already escaped.
///
/// Returns a `'static` pointer -- do not free it. A caught panic returns the
/// same `'static` pointer the success path would have produced.
#[unsafe(no_mangle)]
pub extern "C" fn adele_core_markdown_set_content_function() -> *const c_char {
    /// The `'static` fallback, spelled as a compile-time-checked C string
    /// literal so producing it can never itself panic. Kept in sync with
    /// [`bubble::SET_CONTENT_FUNCTION`] by the pipeline test below.
    static FALLBACK: &std::ffi::CStr = c"adeleSetContent";
    panic_guard::guard(
        "adele_core_markdown_set_content_function",
        || {
            static NAME: OnceLock<CString> = OnceLock::new();
            NAME.get_or_init(|| c_name(bubble::SET_CONTENT_FUNCTION))
                .as_ptr()
        },
        || FALLBACK.as_ptr(),
    )
}

/// Free a string returned by [`adele_core_render_markdown`],
/// [`adele_core_render_markdown_document`] or
/// [`adele_core_markdown_set_content_script`]. Null is a no-op.
///
/// A caught panic returns having freed nothing, the same as a null `text`.
///
/// # Safety
/// `text` must be null, or a pointer returned by one of those three functions
/// and not yet freed. Do not pass the `'static` pointers from the bridge-name
/// accessors.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_string_free(text: *mut c_char) {
    panic_guard::guard(
        "adele_core_string_free",
        move || {
            if text.is_null() {
                return;
            }
            // SAFETY: contract above -- `text` came from `CString::into_raw`, so
            // reclaiming it as a `CString` restores the original allocation and drops it.
            drop(unsafe { CString::from_raw(text) });
        },
        || (),
    );
}

/// NUL-terminate a compile-time bridge name so it can be handed out as a
/// `'static` C pointer.
fn c_name(name: &str) -> CString {
    CString::new(name).expect("bridge names are ASCII identifiers with no interior NUL")
}

// Test-only fault injection for the markdown pipeline.
//
// `adele-markdown` has no reachable panic on untrusted input today (see the
// pipeline audit in the crate's own tests), so there is nothing a malformed
// document can be fed to prove `panic_guard::guard` actually wraps these
// three renderers. This flag forces the next render to panic instead, so the
// wiring itself -- not just the `guard` helper in isolation -- is under test.
// It compiles only under `#[cfg(test)]`, so it is never present in the
// released cdylib.
#[cfg(test)]
thread_local! {
    static FORCE_PANIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Force the next call into the markdown pipeline on this thread to panic.
/// Test-only; see the module comment above `FORCE_PANIC`.
#[cfg(test)]
pub(crate) fn force_next_markdown_panic_for_test() {
    FORCE_PANIC.with(|flag| flag.set(true));
}

#[cfg(not(test))]
#[inline(always)]
fn maybe_panic_for_test() {}

#[cfg(test)]
fn maybe_panic_for_test() {
    if FORCE_PANIC.with(|flag| flag.replace(false)) {
        panic!("injected test panic in the markdown pipeline");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    /// Read a caller-owned C string, free it, and hand back its contents as
    /// an owned `String`.
    ///
    /// # Safety
    /// `ptr` must be non-null and returned by one of this module's
    /// caller-owned-string functions, not yet freed.
    unsafe fn take_c_string(ptr: *mut c_char) -> String {
        assert!(
            !ptr.is_null(),
            "the caller-owned-string doc promises never to return null"
        );
        // SAFETY: contract above.
        let text = unsafe { CStr::from_ptr(ptr) }
            .to_str()
            .expect("valid UTF-8")
            .to_string();
        // SAFETY: `ptr` is freed here exactly once.
        unsafe { adele_core_string_free(ptr) };
        text
    }

    #[test]
    fn adele_core_render_markdown_returns_the_cached_empty_fragment_when_the_pipeline_panics() {
        force_next_markdown_panic_for_test();
        // SAFETY: null is a valid input.
        let rendered = unsafe { take_c_string(adele_core_render_markdown(std::ptr::null())) };
        assert_eq!(rendered, adele_markdown::markdown_to_html(""));
    }

    #[test]
    fn adele_core_render_markdown_document_returns_the_cached_empty_bubble_when_the_pipeline_panics()
     {
        force_next_markdown_panic_for_test();
        // SAFETY: null is a valid input.
        let rendered =
            unsafe { take_c_string(adele_core_render_markdown_document(std::ptr::null())) };
        assert_eq!(rendered, bubble::document(""));
    }

    #[test]
    fn adele_core_markdown_set_content_script_returns_the_clear_statement_when_the_pipeline_panics()
    {
        force_next_markdown_panic_for_test();
        // SAFETY: null is a valid input.
        let rendered =
            unsafe { take_c_string(adele_core_markdown_set_content_script(std::ptr::null())) };
        assert_eq!(rendered, bubble::set_content_script(""));
    }

    #[test]
    fn adele_core_markdown_height_handler_name_matches_the_bubble_module_constant() {
        // SAFETY: this accessor returns a 'static pointer, never freed.
        let name = unsafe { CStr::from_ptr(adele_core_markdown_height_handler_name()) }
            .to_str()
            .expect("ASCII bridge name");
        assert_eq!(name, bubble::HEIGHT_MESSAGE_HANDLER);
    }

    #[test]
    fn adele_core_markdown_set_content_function_matches_the_bubble_module_constant() {
        // SAFETY: this accessor returns a 'static pointer, never freed.
        let name = unsafe { CStr::from_ptr(adele_core_markdown_set_content_function()) }
            .to_str()
            .expect("ASCII bridge name");
        assert_eq!(name, bubble::SET_CONTENT_FUNCTION);
    }

    #[test]
    fn adele_core_string_free_is_a_no_op_on_null() {
        // SAFETY: null is documented as a no-op.
        unsafe { adele_core_string_free(std::ptr::null_mut()) };
    }
}
