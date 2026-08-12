//! Spec for the C-ABI conversation surface.
//!
//! adele-kde and adele-mac drive archiving over this ABI. Both call it from
//! their UI thread with whatever string their model holds, so the entry points
//! must queue and return without blocking and without a panic crossing the
//! boundary — including for a null handle or a null id.

use std::ffi::{CString, c_char, c_void};

use adele_client_core::{
    adele_core_archive_conversation, adele_core_free, adele_core_new,
    adele_core_unarchive_conversation,
};

/// The generated header the C++ consumer includes. Read at compile time, so a
/// header that failed to regenerate cannot pass unnoticed.
const HEADER: &str = include_str!("../include/adele_client_core.h");

extern "C" fn noop_sink(_user_data: *mut c_void, _json: *const c_char) {}

#[test]
fn the_generated_header_declares_the_archive_entry_points() {
    for decl in [
        "void adele_core_archive_conversation(Core *core, const char *conversation_id);",
        "void adele_core_unarchive_conversation(Core *core, const char *conversation_id);",
    ] {
        assert!(HEADER.contains(decl), "missing from the header: {decl}");
    }
}

#[test]
fn archiving_a_conversation_is_accepted_and_returns() {
    let core = adele_core_new(Some(noop_sink), std::ptr::null_mut());
    assert!(!core.is_null(), "a core with a callback must be created");
    let id = CString::new("c1").expect("test id has no interior NUL");
    // SAFETY: `core` is the handle just created; `id` outlives both calls. With
    // no connection the intents are dropped by the actor — what is asserted here
    // is that the calls return rather than block or panic.
    unsafe {
        adele_core_archive_conversation(core, id.as_ptr());
        adele_core_unarchive_conversation(core, id.as_ptr());
        adele_core_free(core);
    }
}

#[test]
fn a_null_handle_or_id_is_ignored_rather_than_fatal() {
    let core = adele_core_new(Some(noop_sink), std::ptr::null_mut());
    // SAFETY: a null handle and a null id are both part of the documented
    // contract — each must be ignored, never dereferenced.
    unsafe {
        adele_core_archive_conversation(std::ptr::null_mut(), std::ptr::null());
        adele_core_unarchive_conversation(std::ptr::null_mut(), std::ptr::null());
        adele_core_archive_conversation(core, std::ptr::null());
        adele_core_unarchive_conversation(core, std::ptr::null());
        adele_core_free(core);
    }
}
