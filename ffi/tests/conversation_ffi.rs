//! Spec for the C-ABI conversation surface.
//!
//! adele-kde and adele-mac drive archiving over this ABI. Both call it from
//! their UI thread with whatever string their model holds, so the entry points
//! must queue and return without blocking and without a panic crossing the
//! boundary - including for a null handle or a null id.
//!
//! One case is driven end to end here, through a live core with no connection:
//! it is the only wiring test the crate can run without a daemon, and it covers
//! the whole path an archive click takes - entry point, intent, executor arm,
//! reducer, view event.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use adele_client_core::{
    adele_core_archive_conversation, adele_core_free, adele_core_new,
    adele_core_unarchive_conversation,
};

/// The generated header the C++ consumer includes, read at compile time.
const HEADER: &str = include_str!("../include/adele_client_core.h");

extern "C" fn noop_sink(_user_data: *mut c_void, _json: *const c_char) {}

/// View-event JSON the recording sink captured, oldest first. A `static`
/// because the sink is an `extern "C" fn` that captures nothing; the case that
/// reads it clears it first and holds [`event_lock`] throughout.
fn recorded() -> &'static Mutex<Vec<String>> {
    static EVENTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}

fn event_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

extern "C" fn recording_sink(_user_data: *mut c_void, json: *const c_char) {
    // SAFETY: the core passes a NUL-terminated string that outlives the call.
    let text = unsafe { CStr::from_ptr(json) }
        .to_string_lossy()
        .into_owned();
    recorded()
        .lock()
        .expect("event buffer is never poisoned")
        .push(text);
}

/// Wait up to a second for a captured event whose JSON contains `needle`.
///
/// The core answers on its own worker thread, so the assertion has to wait for
/// it rather than read straight after the call.
fn waited_for(needle: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let found = recorded()
            .lock()
            .expect("event buffer is never poisoned")
            .iter()
            .find(|json| json.contains(needle))
            .cloned();
        if found.is_some() || Instant::now() > deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_generated_header_declares_the_archive_entry_points() {
    for decl in [
        "void adele_core_archive_conversation(Core *core, const char *conversation_id);",
        "void adele_core_unarchive_conversation(Core *core, const char *conversation_id);",
    ] {
        assert!(HEADER.contains(decl), "missing from the header: {decl}");
    }
}

/// An archive with no connection never reaches a daemon, so what it proves is
/// the wiring: the entry point queues the intent the executor turns into an
/// (un)archive, and the report comes back naming the change the user asked for.
/// Swap the two executor arms and this fails.
#[test]
fn each_entry_point_drives_the_change_it_names() {
    let _guard = event_lock().lock().unwrap_or_else(|e| e.into_inner());
    for (call, expected) in [
        (
            adele_core_archive_conversation as unsafe extern "C" fn(*mut _, *const c_char),
            "not archived",
        ),
        (adele_core_unarchive_conversation, "not unarchived"),
    ] {
        recorded()
            .lock()
            .expect("event buffer is never poisoned")
            .clear();
        let core = adele_core_new(Some(recording_sink), std::ptr::null_mut());
        assert!(!core.is_null(), "a core with a callback must be created");
        let id = CString::new("c1").expect("test id has no interior NUL");
        // SAFETY: `core` is the handle just created and `id` outlives the call.
        unsafe {
            call(core, id.as_ptr());
        }
        let event = waited_for(expected).unwrap_or_else(|| {
            panic!(
                "no event reported the change as {expected}: {:?}",
                recorded().lock().expect("event buffer is never poisoned")
            )
        });
        assert!(event.contains("Not connected"), "{event}");
        // SAFETY: the handle is live and is not used again.
        unsafe { adele_core_free(core) };
    }
}

#[test]
fn a_null_handle_is_ignored_and_a_null_id_is_not_fatal() {
    let core = adele_core_new(Some(noop_sink), std::ptr::null_mut());
    // SAFETY: a null handle and a null id are both part of the documented
    // contract - the handle is never dereferenced, and a null id decodes to an
    // empty string, which the daemon refuses like any unknown id.
    unsafe {
        adele_core_archive_conversation(std::ptr::null_mut(), std::ptr::null());
        adele_core_unarchive_conversation(std::ptr::null_mut(), std::ptr::null());
        adele_core_archive_conversation(core, std::ptr::null());
        adele_core_unarchive_conversation(core, std::ptr::null());
        adele_core_free(core);
    }
}
