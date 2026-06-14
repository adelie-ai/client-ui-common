//! `libadele_client_core` — the native C ABI for C/C++ Adelie clients.
//!
//! A thin, panic-free C surface over the shared **`client-ui-common`** reducer
//! (the same `WindowState` state machine gtk/tui run) plus a **`client-common`
//! `Connector`** — by default in **D-Bus mode** (the `org.desktopAssistant`
//! bridge), the canonical KDE transport. The model + controller + transport all
//! live here in safe Rust; the C++/QML side is glue only.
//!
//! # Shape of the ABI
//!
//! - [`adele_core_new`] takes a [`ViewCallback`] + `user_data` and returns an
//!   opaque `AdeleCore *`. The callback is invoked (on a worker thread) with a
//!   JSON `ViewEvent` string for every view update — see `view_event.rs` for the
//!   `{"type": ...}` schema. The C++ side marshals each onto its UI thread.
//! - The `adele_core_*` action functions queue work; they return immediately and
//!   never block the caller. Results arrive later via the callback.
//! - [`adele_core_free`] tears everything down.
//!
//! # Threading
//!
//! The callback fires on a core worker thread. Marshal to the UI thread before
//! touching widgets (e.g. `QMetaObject::invokeMethod(obj, ..., Qt::QueuedConnection)`).
//! All string arguments are borrowed for the duration of the call and copied;
//! the caller retains ownership.

mod engine;
mod view_event;

use std::ffi::{CStr, c_char, c_void};

use desktop_assistant_client_common::TransportMode;

use crate::engine::{Core, Intent, ViewSink};
use crate::view_event::adele_output_from_str;

/// Decode a borrowed C string into an owned `String`. `null` ⇒ empty; invalid
/// UTF-8 ⇒ lossily replaced — never panics.
///
/// # Safety
/// `ptr` must be null or point to a valid NUL-terminated C string that stays
/// valid for the duration of the call.
unsafe fn cstr_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: contract above — `ptr` is a valid NUL-terminated string.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Create a core instance. `callback` receives view-event JSON strings;
/// `user_data` is passed back to it verbatim (carry your C++ `this` here).
/// Returns an opaque handle, or null if `callback` is null. Free it with
/// [`adele_core_free`].
///
/// The callback type is spelled inline (rather than via the `ViewCallback`
/// alias) so cbindgen emits a real nullable C function pointer rather than an
/// opaque struct; `Option` is what lets Rust accept a null pointer safely.
#[unsafe(no_mangle)]
pub extern "C" fn adele_core_new(
    callback: Option<extern "C" fn(user_data: *mut c_void, json: *const c_char)>,
    user_data: *mut c_void,
) -> *mut Core {
    let Some(callback) = callback else {
        return std::ptr::null_mut();
    };
    let sink = ViewSink::new(callback, user_data as usize);
    Box::into_raw(Box::new(Core::new(sink)))
}

/// Destroy a core instance, shutting down its runtime and connection.
///
/// # Safety
/// `core` must be a handle returned by [`adele_core_new`] (or null), and must
/// not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_free(core: *mut Core) {
    if core.is_null() {
        return;
    }
    // SAFETY: `core` came from `Box::into_raw` in `adele_core_new`.
    drop(unsafe { Box::from_raw(core) });
}

/// Connect to the daemon. `transport` is `"dbus"` (default for anything
/// unrecognised), `"uds"`, or `"ws"`; `address` is the UDS socket path or WS url
/// (empty for the default), ignored for D-Bus.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]; `transport`/`address`
/// must be null or valid NUL-terminated C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_connect(
    core: *mut Core,
    transport: *const c_char,
    address: *const c_char,
) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    let mode = match unsafe { cstr_to_string(transport) }.as_str() {
        "ws" => TransportMode::Ws,
        "uds" => TransportMode::Uds,
        _ => TransportMode::Dbus,
    };
    let address = unsafe { cstr_to_string(address) };
    core.send_intent(Intent::Connect { mode, address });
}

/// Send a prompt into the open conversation.
///
/// # Safety
/// `core` must be a live handle; `text` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_send_prompt(core: *mut Core, text: *const c_char) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SendPrompt(unsafe { cstr_to_string(text) }));
}

/// Open (load) a conversation by id.
///
/// # Safety
/// `core` must be a live handle; `conversation_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_select_conversation(
    core: *mut Core,
    conversation_id: *const c_char,
) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SelectConversation(unsafe {
        cstr_to_string(conversation_id)
    }));
}

/// Create a new conversation and open it.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_new_conversation(core: *mut Core) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::NewConversation);
}

/// Delete a conversation by id.
///
/// # Safety
/// `core` must be a live handle; `conversation_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_delete_conversation(
    core: *mut Core,
    conversation_id: *const c_char,
) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::DeleteConversation(unsafe {
        cstr_to_string(conversation_id)
    }));
}

/// Set the `You:` (voice input) state for a conversation.
///
/// # Safety
/// `core` must be a live handle; `conversation_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_voice_in(
    core: *mut Core,
    conversation_id: *const c_char,
    enabled: bool,
) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SetVoiceIn {
        conversation_id: unsafe { cstr_to_string(conversation_id) },
        enabled,
    });
}

/// Set the `Adele:` (voice output) level for a conversation. `level` is
/// `"disabled"`, `"on_demand"`, or `"always"` (anything else ⇒ `"disabled"`).
///
/// # Safety
/// `core` must be a live handle; `conversation_id`/`level` must be null or valid
/// C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_adele_output(
    core: *mut Core,
    conversation_id: *const c_char,
    level: *const c_char,
) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    let level = adele_output_from_str(&unsafe { cstr_to_string(level) });
    core.send_intent(Intent::SetAdeleOutput {
        conversation_id: unsafe { cstr_to_string(conversation_id) },
        level,
    });
}
