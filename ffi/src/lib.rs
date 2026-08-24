//! `libadele_client_core` -- the native C ABI for C/C++ Adelie clients.
//!
//! A thin C surface over the shared **`client-ui-common`** reducer (the same
//! `WindowState` state machine gtk/tui run) plus a **`client-common`
//! `Connector`** -- by default in **D-Bus mode** (the `org.desktopAssistant`
//! bridge), the canonical KDE transport. The model + controller + transport all
//! live here in safe Rust; the C++/QML side is glue only.
//!
//! Every `extern "C"` entry point in this crate runs its body behind this
//! crate's internal `panic_guard::guard` helper, so a Rust panic never
//! unwinds into the caller. A caught panic is logged (payload and source
//! location) and the call returns the documented neutral value instead --
//! see each function's doc comment for what that value is.
//!
//! # Shape of the ABI
//!
//! - [`adele_core_new`] takes a `ViewCallback` + `user_data` and returns an
//!   opaque `AdeleCore *`. The callback is invoked (on a worker thread) with a
//!   JSON `ViewEvent` string for every view update -- see `view_event.rs` for the
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

mod builtins;
mod client_mcp;
mod conversations;
mod engine;
#[cfg(test)]
mod entry_point_coverage;
mod markdown;
mod panic_guard;
mod view_event;

// The markdown surface is `no_mangle`, so the cdylib exports it either way;
// re-exporting keeps it reachable by path for the rlib consumers (the spec).
pub use markdown::{
    adele_core_markdown_height_handler_name, adele_core_markdown_set_content_function,
    adele_core_markdown_set_content_script, adele_core_markdown_set_content_script_n,
    adele_core_render_markdown, adele_core_render_markdown_document,
    adele_core_render_markdown_document_n, adele_core_render_markdown_n, adele_core_string_free,
};

use std::ffi::{CStr, c_char, c_void};

use desktop_assistant_client_common::TransportMode;

use crate::client_mcp::ClientServerWrite;
use crate::engine::{Core, Intent, ViewSink};
use crate::view_event::adele_output_from_str;

/// Decode a borrowed C string into an owned `String`. `null` -> empty; invalid
/// UTF-8 -> lossily replaced -- never panics.
///
/// # Safety
/// `ptr` must be null or point to a valid NUL-terminated C string that stays
/// valid for the duration of the call.
pub(crate) unsafe fn cstr_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: contract above -- `ptr` is a valid NUL-terminated string.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Decode a borrowed, length-carrying C string into an owned `String`. `ptr`
/// need not be NUL-terminated and any embedded NUL byte inside `len` is kept,
/// not truncated at. `null` (any `len`) and `len == 0` (any `ptr`, not
/// dereferenced) both decode as the empty string. Invalid UTF-8 is replaced
/// lossily, matching [`cstr_to_string`].
///
/// A `len` longer than `ptr`'s true allocation is the caller's error. This
/// function has no way to detect that -- it trusts `len` the same way
/// `strncpy(dst, src, n)` trusts its caller's `n` -- so an over-long `len`
/// reads past the allocation and the behaviour is undefined, not a
/// documented failure mode.
///
/// # Safety
/// `len == 0` returns before `ptr` is read at all, so a null or dangling
/// `ptr` is sound in that case. Otherwise, `ptr` must be non-null and point
/// to at least `len` readable bytes, valid for the duration of the call.
pub(crate) unsafe fn cstr_n_to_string(ptr: *const c_char, len: usize) -> String {
    if len == 0 {
        return String::new();
    }
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: `len != 0` and `ptr` is non-null per the checks above; the
    // caller contract on this function requires `ptr` to point to at least
    // `len` readable bytes for the duration of the call.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Create a core instance. `callback` receives view-event JSON strings;
/// `user_data` is passed back to it verbatim (carry your C++ `this` here).
/// Returns an opaque handle, or null if `callback` is null, if the runtime
/// could not be constructed, or if a caught panic swallowed the attempt.
/// Free the handle with [`adele_core_free`].
///
/// The callback type is spelled inline (rather than via the `ViewCallback`
/// alias) so cbindgen emits a real nullable C function pointer rather than an
/// opaque struct; `Option` is what lets Rust accept a null pointer safely.
#[unsafe(no_mangle)]
pub extern "C" fn adele_core_new(
    callback: Option<extern "C" fn(user_data: *mut c_void, json: *const c_char)>,
    user_data: *mut c_void,
) -> *mut Core {
    panic_guard::guard(
        "adele_core_new",
        move || {
            let Some(callback) = callback else {
                return std::ptr::null_mut();
            };
            let sink = ViewSink::new(callback, user_data as usize);
            match Core::new(sink) {
                Some(core) => Box::into_raw(Box::new(core)),
                None => std::ptr::null_mut(),
            }
        },
        std::ptr::null_mut,
    )
}

/// Destroy a core instance, shutting down its runtime and connection.
///
/// A caught panic returns having freed nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a handle returned by [`adele_core_new`] (or null), and must
/// not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_free(core: *mut Core) {
    panic_guard::guard(
        "adele_core_free",
        move || {
            if core.is_null() {
                return;
            }
            // SAFETY: `core` came from `Box::into_raw` in `adele_core_new`.
            drop(unsafe { Box::from_raw(core) });
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_connect`] and [`adele_core_connect_n`]:
/// resolve the transport mode and queue the connect intent.
fn connect_impl(core: *mut Core, transport: String, address: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    let mode = match transport.as_str() {
        "ws" => TransportMode::Ws,
        "uds" => TransportMode::Uds,
        _ => TransportMode::Dbus,
    };
    core.send_intent(Intent::Connect { mode, address });
}

/// Connect to the daemon. `transport` is `"dbus"` (default for anything
/// unrecognised), `"uds"`, or `"ws"`; `address` is the UDS socket path or WS url
/// (empty for the default), ignored for D-Bus.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
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
    panic_guard::guard(
        "adele_core_connect",
        move || {
            // SAFETY: C caller guarantees pointers are valid NUL-terminated strings
            // for the duration of this call.
            let transport = unsafe { cstr_to_string(transport) };
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let address = unsafe { cstr_to_string(address) };
            connect_impl(core, transport, address);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_connect`]. `transport_len`/`address_len`
/// are the number of bytes at `transport`/`address` -- see [`cstr_n_to_string`]
/// for the exact decode semantics (null/zero-length -> empty, embedded NUL
/// kept, invalid UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]. `transport`/`address`
/// must be null (with any length), or point to at least
/// `transport_len`/`address_len` readable bytes for the duration of the call.
/// A length longer than the pointer's true allocation is the caller's error
/// and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_connect_n(
    core: *mut Core,
    transport: *const c_char,
    transport_len: usize,
    address: *const c_char,
    address_len: usize,
) {
    panic_guard::guard(
        "adele_core_connect_n",
        move || {
            // SAFETY: contract above.
            let transport = unsafe { cstr_n_to_string(transport, transport_len) };
            // SAFETY: contract above.
            let address = unsafe { cstr_n_to_string(address, address_len) };
            connect_impl(core, transport, address);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_send_prompt`] and
/// [`adele_core_send_prompt_n`]: queue the send-prompt intent.
fn send_prompt_impl(core: *mut Core, text: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SendPrompt(text));
}

/// Send a prompt into the open conversation.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle; `text` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_send_prompt(core: *mut Core, text: *const c_char) {
    panic_guard::guard(
        "adele_core_send_prompt",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let text = unsafe { cstr_to_string(text) };
            send_prompt_impl(core, text);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_send_prompt`]. `text_len` is the
/// number of bytes at `text` -- see [`cstr_n_to_string`] for the exact decode
/// semantics (null/zero-length -> empty, embedded NUL kept, invalid UTF-8
/// replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle. `text` must be null (with any length), or
/// point to at least `text_len` readable bytes for the duration of the call.
/// A length longer than the pointer's true allocation is the caller's error
/// and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_send_prompt_n(
    core: *mut Core,
    text: *const c_char,
    text_len: usize,
) {
    panic_guard::guard(
        "adele_core_send_prompt_n",
        move || {
            // SAFETY: contract above.
            let text = unsafe { cstr_n_to_string(text, text_len) };
            send_prompt_impl(core, text);
        },
        || (),
    );
}

/// Check out queued message `index` into the composer to edit it (up-arrow
/// recall / a chip's edit affordance). The text loads via a `composer_text` view
/// event; re-submitting reinserts it in place. An out-of-range index is ignored.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_edit_queued(core: *mut Core, index: usize) {
    panic_guard::guard(
        "adele_core_edit_queued",
        move || {
            // SAFETY: contract above.
            let Some(core) = (unsafe { core.as_ref() }) else {
                return;
            };
            core.send_intent(Intent::EditQueued(index));
        },
        || (),
    );
}

/// Remove queued message `index` (a chip's x) without sending it. An
/// out-of-range index is ignored.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_remove_queued(core: *mut Core, index: usize) {
    panic_guard::guard(
        "adele_core_remove_queued",
        move || {
            // SAFETY: contract above.
            let Some(core) = (unsafe { core.as_ref() }) else {
                return;
            };
            core.send_intent(Intent::RemoveQueued(index));
        },
        || (),
    );
}

/// Abandon an in-progress queued-message edit: the checked-out message returns
/// to the queue unchanged and the composer clears. A no-op when not editing.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_cancel_queued_edit(core: *mut Core) {
    panic_guard::guard(
        "adele_core_cancel_queued_edit",
        move || {
            // SAFETY: contract above.
            let Some(core) = (unsafe { core.as_ref() }) else {
                return;
            };
            core.send_intent(Intent::CancelQueuedEdit);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_select_conversation`] and
/// [`adele_core_select_conversation_n`]: queue the select-conversation intent.
fn select_conversation_impl(core: *mut Core, conversation_id: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SelectConversation(conversation_id));
}

/// Open (load) a conversation by id.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle; `conversation_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_select_conversation(
    core: *mut Core,
    conversation_id: *const c_char,
) {
    panic_guard::guard(
        "adele_core_select_conversation",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let conversation_id = unsafe { cstr_to_string(conversation_id) };
            select_conversation_impl(core, conversation_id);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_select_conversation`].
/// `conversation_id_len` is the number of bytes at `conversation_id` -- see
/// [`cstr_n_to_string`] for the exact decode semantics (null/zero-length ->
/// empty, embedded NUL kept, invalid UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle. `conversation_id` must be null (with any
/// length), or point to at least `conversation_id_len` readable bytes for the
/// duration of the call. A length longer than the pointer's true allocation
/// is the caller's error and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_select_conversation_n(
    core: *mut Core,
    conversation_id: *const c_char,
    conversation_id_len: usize,
) {
    panic_guard::guard(
        "adele_core_select_conversation_n",
        move || {
            // SAFETY: contract above.
            let conversation_id = unsafe { cstr_n_to_string(conversation_id, conversation_id_len) };
            select_conversation_impl(core, conversation_id);
        },
        || (),
    );
}

/// Create a new conversation and open it.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_new_conversation(core: *mut Core) {
    panic_guard::guard(
        "adele_core_new_conversation",
        move || {
            // SAFETY: contract above.
            let Some(core) = (unsafe { core.as_ref() }) else {
                return;
            };
            core.send_intent(Intent::NewConversation);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_delete_conversation`] and
/// [`adele_core_delete_conversation_n`]: queue the delete-conversation intent.
fn delete_conversation_impl(core: *mut Core, conversation_id: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::DeleteConversation(conversation_id));
}

/// Delete a conversation by id.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle; `conversation_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_delete_conversation(
    core: *mut Core,
    conversation_id: *const c_char,
) {
    panic_guard::guard(
        "adele_core_delete_conversation",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let conversation_id = unsafe { cstr_to_string(conversation_id) };
            delete_conversation_impl(core, conversation_id);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_delete_conversation`].
/// `conversation_id_len` is the number of bytes at `conversation_id` -- see
/// [`cstr_n_to_string`] for the exact decode semantics (null/zero-length ->
/// empty, embedded NUL kept, invalid UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle. `conversation_id` must be null (with any
/// length), or point to at least `conversation_id_len` readable bytes for the
/// duration of the call. A length longer than the pointer's true allocation
/// is the caller's error and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_delete_conversation_n(
    core: *mut Core,
    conversation_id: *const c_char,
    conversation_id_len: usize,
) {
    panic_guard::guard(
        "adele_core_delete_conversation_n",
        move || {
            // SAFETY: contract above.
            let conversation_id = unsafe { cstr_n_to_string(conversation_id, conversation_id_len) };
            delete_conversation_impl(core, conversation_id);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_archive_conversation`] and
/// [`adele_core_archive_conversation_n`]: queue the archive-conversation
/// intent.
fn archive_conversation_impl(core: *mut Core, conversation_id: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::ArchiveConversation(conversation_id));
}

/// Put a conversation away. The core performs the change and then re-reads the
/// conversation list, so the refreshed inventory arrives as a `conversations`
/// view event with no further call from the client. Each row carries `archived`,
/// so a client groups or hides them as it sees fit.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle; `conversation_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_archive_conversation(
    core: *mut Core,
    conversation_id: *const c_char,
) {
    panic_guard::guard(
        "adele_core_archive_conversation",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let conversation_id = unsafe { cstr_to_string(conversation_id) };
            archive_conversation_impl(core, conversation_id);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_archive_conversation`].
/// `conversation_id_len` is the number of bytes at `conversation_id` -- see
/// [`cstr_n_to_string`] for the exact decode semantics (null/zero-length ->
/// empty, embedded NUL kept, invalid UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle. `conversation_id` must be null (with any
/// length), or point to at least `conversation_id_len` readable bytes for the
/// duration of the call. A length longer than the pointer's true allocation
/// is the caller's error and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_archive_conversation_n(
    core: *mut Core,
    conversation_id: *const c_char,
    conversation_id_len: usize,
) {
    panic_guard::guard(
        "adele_core_archive_conversation_n",
        move || {
            // SAFETY: contract above.
            let conversation_id = unsafe { cstr_n_to_string(conversation_id, conversation_id_len) };
            archive_conversation_impl(core, conversation_id);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_unarchive_conversation`] and
/// [`adele_core_unarchive_conversation_n`]: queue the unarchive-conversation
/// intent.
fn unarchive_conversation_impl(core: *mut Core, conversation_id: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::UnarchiveConversation(conversation_id));
}

/// Bring an archived conversation back out. Refreshes the list exactly as
/// [`adele_core_archive_conversation`] does.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle; `conversation_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_unarchive_conversation(
    core: *mut Core,
    conversation_id: *const c_char,
) {
    panic_guard::guard(
        "adele_core_unarchive_conversation",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let conversation_id = unsafe { cstr_to_string(conversation_id) };
            unarchive_conversation_impl(core, conversation_id);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_unarchive_conversation`].
/// `conversation_id_len` is the number of bytes at `conversation_id` -- see
/// [`cstr_n_to_string`] for the exact decode semantics (null/zero-length ->
/// empty, embedded NUL kept, invalid UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle. `conversation_id` must be null (with any
/// length), or point to at least `conversation_id_len` readable bytes for the
/// duration of the call. A length longer than the pointer's true allocation
/// is the caller's error and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_unarchive_conversation_n(
    core: *mut Core,
    conversation_id: *const c_char,
    conversation_id_len: usize,
) {
    panic_guard::guard(
        "adele_core_unarchive_conversation_n",
        move || {
            // SAFETY: contract above.
            let conversation_id = unsafe { cstr_n_to_string(conversation_id, conversation_id_len) };
            unarchive_conversation_impl(core, conversation_id);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_set_voice_in`] and
/// [`adele_core_set_voice_in_n`]: queue the set-voice-in intent.
fn set_voice_in_impl(core: *mut Core, conversation_id: String, enabled: bool) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SetVoiceIn {
        conversation_id,
        enabled,
    });
}

/// Set the `You:` (voice input) state for a conversation.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle; `conversation_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_voice_in(
    core: *mut Core,
    conversation_id: *const c_char,
    enabled: bool,
) {
    panic_guard::guard(
        "adele_core_set_voice_in",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let conversation_id = unsafe { cstr_to_string(conversation_id) };
            set_voice_in_impl(core, conversation_id, enabled);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_set_voice_in`]. `conversation_id_len`
/// is the number of bytes at `conversation_id` -- see [`cstr_n_to_string`] for
/// the exact decode semantics (null/zero-length -> empty, embedded NUL kept,
/// invalid UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle. `conversation_id` must be null (with any
/// length), or point to at least `conversation_id_len` readable bytes for the
/// duration of the call. A length longer than the pointer's true allocation
/// is the caller's error and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_voice_in_n(
    core: *mut Core,
    conversation_id: *const c_char,
    conversation_id_len: usize,
    enabled: bool,
) {
    panic_guard::guard(
        "adele_core_set_voice_in_n",
        move || {
            // SAFETY: contract above.
            let conversation_id = unsafe { cstr_n_to_string(conversation_id, conversation_id_len) };
            set_voice_in_impl(core, conversation_id, enabled);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_set_adele_output`] and
/// [`adele_core_set_adele_output_n`]: resolve the output level and queue the
/// intent.
fn set_adele_output_impl(core: *mut Core, conversation_id: String, level: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    let level = adele_output_from_str(&level);
    core.send_intent(Intent::SetAdeleOutput {
        conversation_id,
        level,
    });
}

/// Set the `Adele:` (voice output) level for a conversation. `level` is
/// `"disabled"`, `"on_demand"`, or `"always"` (anything else -> `"disabled"`).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
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
    panic_guard::guard(
        "adele_core_set_adele_output",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let conversation_id = unsafe { cstr_to_string(conversation_id) };
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let level = unsafe { cstr_to_string(level) };
            set_adele_output_impl(core, conversation_id, level);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_set_adele_output`].
/// `conversation_id_len`/`level_len` are the number of bytes at
/// `conversation_id`/`level` -- see [`cstr_n_to_string`] for the exact decode
/// semantics (null/zero-length -> empty, embedded NUL kept, invalid UTF-8
/// replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle. `conversation_id`/`level` must each be null
/// (with any length), or point to at least
/// `conversation_id_len`/`level_len` readable bytes for the duration of the
/// call. A length longer than the pointer's true allocation is the caller's
/// error and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_adele_output_n(
    core: *mut Core,
    conversation_id: *const c_char,
    conversation_id_len: usize,
    level: *const c_char,
    level_len: usize,
) {
    panic_guard::guard(
        "adele_core_set_adele_output_n",
        move || {
            // SAFETY: contract above.
            let conversation_id = unsafe { cstr_n_to_string(conversation_id, conversation_id_len) };
            // SAFETY: contract above.
            let level = unsafe { cstr_n_to_string(level, level_len) };
            set_adele_output_impl(core, conversation_id, level);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_select_model`] and
/// [`adele_core_select_model_n`]: queue the select-model intent.
fn select_model_impl(core: *mut Core, connection_id: String, model_id: String, effort: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SelectModel {
        connection_id,
        model_id,
        effort,
    });
}

/// Stage (or clear) a per-message model override for the next send. Empty
/// `connection_id`/`model_id` clears it (inherit the default); `effort` is
/// `"low"`/`"medium"`/`"high"` or empty (no effort hint).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle; the string args must be null or valid C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_select_model(
    core: *mut Core,
    connection_id: *const c_char,
    model_id: *const c_char,
    effort: *const c_char,
) {
    panic_guard::guard(
        "adele_core_select_model",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let connection_id = unsafe { cstr_to_string(connection_id) };
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let model_id = unsafe { cstr_to_string(model_id) };
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let effort = unsafe { cstr_to_string(effort) };
            select_model_impl(core, connection_id, model_id, effort);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_select_model`].
/// `connection_id_len`/`model_id_len`/`effort_len` are the number of bytes at
/// `connection_id`/`model_id`/`effort` -- see [`cstr_n_to_string`] for the
/// exact decode semantics (null/zero-length -> empty, embedded NUL kept,
/// invalid UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle. `connection_id`/`model_id`/`effort` must
/// each be null (with any length), or point to at least their matching
/// `_len` readable bytes for the duration of the call. A length longer than
/// the pointer's true allocation is the caller's error and is not detectable
/// here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_select_model_n(
    core: *mut Core,
    connection_id: *const c_char,
    connection_id_len: usize,
    model_id: *const c_char,
    model_id_len: usize,
    effort: *const c_char,
    effort_len: usize,
) {
    panic_guard::guard(
        "adele_core_select_model_n",
        move || {
            // SAFETY: contract above.
            let connection_id = unsafe { cstr_n_to_string(connection_id, connection_id_len) };
            // SAFETY: contract above.
            let model_id = unsafe { cstr_n_to_string(model_id, model_id_len) };
            // SAFETY: contract above.
            let effort = unsafe { cstr_n_to_string(effort, effort_len) };
            select_model_impl(core, connection_id, model_id, effort);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_cancel_task`] and
/// [`adele_core_cancel_task_n`]: queue the cancel-task intent.
fn cancel_task_impl(core: *mut Core, task_id: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::CancelTask(task_id));
}

/// Request cancellation of a background task by id.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle; `task_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_cancel_task(core: *mut Core, task_id: *const c_char) {
    panic_guard::guard(
        "adele_core_cancel_task",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let task_id = unsafe { cstr_to_string(task_id) };
            cancel_task_impl(core, task_id);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_cancel_task`]. `task_id_len` is the
/// number of bytes at `task_id` -- see [`cstr_n_to_string`] for the exact
/// decode semantics (null/zero-length -> empty, embedded NUL kept, invalid
/// UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle. `task_id` must be null (with any length),
/// or point to at least `task_id_len` readable bytes for the duration of the
/// call. A length longer than the pointer's true allocation is the caller's
/// error and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_cancel_task_n(
    core: *mut Core,
    task_id: *const c_char,
    task_id_len: usize,
) {
    panic_guard::guard(
        "adele_core_cancel_task_n",
        move || {
            // SAFETY: contract above.
            let task_id = unsafe { cstr_n_to_string(task_id, task_id_len) };
            cancel_task_impl(core, task_id);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_fetch_task_logs`] and
/// [`adele_core_fetch_task_logs_n`]: queue the fetch-task-logs intent.
fn fetch_task_logs_impl(core: *mut Core, task_id: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::FetchTaskLogs(task_id));
}

/// Fetch a background task's log page; the result arrives later as a `task_logs`
/// view event.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle; `task_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_fetch_task_logs(core: *mut Core, task_id: *const c_char) {
    panic_guard::guard(
        "adele_core_fetch_task_logs",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let task_id = unsafe { cstr_to_string(task_id) };
            fetch_task_logs_impl(core, task_id);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_fetch_task_logs`]. `task_id_len` is
/// the number of bytes at `task_id` -- see [`cstr_n_to_string`] for the exact
/// decode semantics (null/zero-length -> empty, embedded NUL kept, invalid
/// UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle. `task_id` must be null (with any length),
/// or point to at least `task_id_len` readable bytes for the duration of the
/// call. A length longer than the pointer's true allocation is the caller's
/// error and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_fetch_task_logs_n(
    core: *mut Core,
    task_id: *const c_char,
    task_id_len: usize,
) {
    panic_guard::guard(
        "adele_core_fetch_task_logs_n",
        move || {
            // SAFETY: contract above.
            let task_id = unsafe { cstr_n_to_string(task_id, task_id_len) };
            fetch_task_logs_impl(core, task_id);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_set_ws_jwt`] and
/// [`adele_core_set_ws_jwt_n`]: queue the set-ws-jwt intent.
fn set_ws_jwt_impl(core: *mut Core, jwt: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SetWsJwt(jwt));
}

/// Stage an explicit WebSocket bearer token for the next [`adele_core_connect`]
/// (empty -> clear). Used verbatim as the WS bearer credential, bypassing the
/// D-Bus / `/login` token minting -- the path a client with no local token minter
/// (e.g. macOS, which has no D-Bus bridge) uses after obtaining a token
/// out-of-band from the daemon's `/login`. Ignored for non-WS transports.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle; `jwt` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_ws_jwt(core: *mut Core, jwt: *const c_char) {
    panic_guard::guard(
        "adele_core_set_ws_jwt",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let jwt = unsafe { cstr_to_string(jwt) };
            set_ws_jwt_impl(core, jwt);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_set_ws_jwt`]. `jwt_len` is the number
/// of bytes at `jwt` -- see [`cstr_n_to_string`] for the exact decode
/// semantics (null/zero-length -> empty, embedded NUL kept, invalid UTF-8
/// replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle. `jwt` must be null (with any length), or
/// point to at least `jwt_len` readable bytes for the duration of the call.
/// A length longer than the pointer's true allocation is the caller's error
/// and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_ws_jwt_n(
    core: *mut Core,
    jwt: *const c_char,
    jwt_len: usize,
) {
    panic_guard::guard(
        "adele_core_set_ws_jwt_n",
        move || {
            // SAFETY: contract above.
            let jwt = unsafe { cstr_n_to_string(jwt, jwt_len) };
            set_ws_jwt_impl(core, jwt);
        },
        || (),
    );
}

/// Set whether basic device context (name, username, home dir, hostname,
/// timezone, OS) is shared with the assistant on the next [`adele_core_connect`]
/// (#549). `true` (the default) shares it so the assistant can personalize;
/// `false` opts out, sending no context field / header at all. Staged on the
/// core and applied when the next connect builds its `ConnectionConfig`, so a
/// change takes effect on the following (re)connect. This backs the KDE KCM
/// "Share device info with the assistant" checkbox.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_share_client_context(core: *mut Core, enabled: bool) {
    panic_guard::guard(
        "adele_core_set_share_client_context",
        move || {
            // SAFETY: contract above.
            let Some(core) = (unsafe { core.as_ref() }) else {
                return;
            };
            core.send_intent(Intent::SetShareClientContext(enabled));
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_set_mcp_surface`] and
/// [`adele_core_set_mcp_surface_n`]: queue the set-mcp-surface intent.
fn set_mcp_surface_impl(core: *mut Core, surface: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SetMcpSurface(surface));
}

/// Declare which `client-mcp.toml` surface this client resolves its MCP servers
/// (and `disabled_builtins`) under -- `"mac"`, `"kde"`, ... Server *definitions* are
/// machine-wide; the surface is the per-client enable layer, so one set of
/// servers can be configured once and switched on per client.
///
/// Call this once before [`adele_core_connect`]; it is read when the connect
/// starts the client MCP host, so a later change applies on the next
/// (re)connect. A NULL or empty name is ignored and the core keeps its default
/// surface (`kde`), which is what adele-kde relies on by never calling this.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]; `surface` must be NULL
/// or a valid NUL-terminated UTF-8 string, borrowed for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_mcp_surface(core: *mut Core, surface: *const c_char) {
    panic_guard::guard(
        "adele_core_set_mcp_surface",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let surface = unsafe { cstr_to_string(surface) };
            set_mcp_surface_impl(core, surface);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_set_mcp_surface`]. `surface_len` is
/// the number of bytes at `surface` -- see [`cstr_n_to_string`] for the exact
/// decode semantics (null/zero-length -> empty, embedded NUL kept, invalid
/// UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]. `surface` must be
/// null (with any length), or point to at least `surface_len` readable bytes
/// for the duration of the call. A length longer than the pointer's true
/// allocation is the caller's error and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_mcp_surface_n(
    core: *mut Core,
    surface: *const c_char,
    surface_len: usize,
) {
    panic_guard::guard(
        "adele_core_set_mcp_surface_n",
        move || {
            // SAFETY: contract above.
            let surface = unsafe { cstr_n_to_string(surface, surface_len) };
            set_mcp_surface_impl(core, surface);
        },
        || (),
    );
}

/// Ask for this client's compiled-in ("built-in") MCP servers and their status
/// under the surface declared via [`adele_core_set_mcp_surface`]. The answer
/// arrives as an `mcp_builtins` view event carrying, per server: `name`,
/// `namespace`, `kind`, `tool_count`, `overridden_by` (the same-name external
/// server shadowing it, or null), and `disabled_by_config` (this surface's
/// opt-out).
///
/// Answerable with **no connection**: which servers are built in is a property of
/// how this cdylib was built (`--features mcp-*`) plus what `client-mcp.toml`
/// says, so a settings panel can call this before the first connect. A core built
/// with no `mcp-*` feature -- adele-kde's -- answers with an empty list, which is
/// the honest "none linked in" rather than a missing reply.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_request_mcp_builtins(core: *mut Core) {
    panic_guard::guard(
        "adele_core_request_mcp_builtins",
        move || {
            // SAFETY: contract above.
            let Some(core) = (unsafe { core.as_ref() }) else {
                return;
            };
            core.send_intent(Intent::RequestMcpBuiltins);
        },
        || (),
    );
}

/// Ask for this client's external client-run MCP servers -- the `client-mcp.toml`
/// servers the surface declared via [`adele_core_set_mcp_surface`] hosts on the
/// edge -- and their status. The answer arrives as an `mcp_client_servers` view
/// event carrying, per server: `name`, `transport` (`stdio`/`http`), `status`,
/// `tool_count`, and `namespace` (or null).
///
/// The sibling of [`adele_core_request_mcp_builtins`], and like it answerable with
/// **no connection**: which external servers this machine defines, and which of
/// them this surface hosts, are both properties of `client-mcp.toml`, so a
/// settings panel can call this before the first connect.
///
/// The list covers every **defined** server, not only the hosted ones, so a panel
/// can show -- and switch back on -- a server this surface has turned off. A server
/// this surface does not host reports `disabled`. A hosted one reports `enabled`,
/// with a `0` tool count, until a running client MCP host has been started with
/// it -- so a server added or switched on during a connection also reports
/// `enabled`, because the running host was never given it and it starts on the
/// next [`adele_core_connect`]. Once a running host has been given it, the row is
/// `running` with its live tool count, or `error` when the host could not start
/// it. A machine that defines no external servers answers with an empty list --
/// the honest "none configured".
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_request_mcp_client_servers(core: *mut Core) {
    panic_guard::guard(
        "adele_core_request_mcp_client_servers",
        move || {
            // SAFETY: contract above.
            let Some(core) = (unsafe { core.as_ref() }) else {
                return;
            };
            core.send_intent(Intent::RequestMcpClientServers);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_set_mcp_builtin_disabled`] and
/// [`adele_core_set_mcp_builtin_disabled_n`]: queue the
/// set-mcp-builtin-disabled intent.
fn set_mcp_builtin_disabled_impl(core: *mut Core, name: String, disabled: bool) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SetMcpBuiltinDisabled { name, disabled });
}

/// Turn one built-in MCP server off (`disabled = true`) or back on for **this
/// client's surface**, by writing `[surfaces.<surface>].disabled_builtins` in the
/// shared `client-mcp.toml`.
///
/// The write goes through the core because that file is machine-wide: every Adele
/// client on the box reads the same one, and a second independent writer would be
/// a correctness hazard for all of them. Only the declared surface's section is
/// touched, so opting out here never disturbs another client's selection.
///
/// Takes effect on the next [`adele_core_connect`] -- a running MCP host is fixed
/// at start. An `mcp_builtins` view event follows either way (including on
/// failure, which also emits a `toast`), carrying the pending state so the panel
/// stays honest in the meantime. A NULL or empty `name` is refused.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]; `name` must be NULL or a
/// valid NUL-terminated UTF-8 string, borrowed for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_mcp_builtin_disabled(
    core: *mut Core,
    name: *const c_char,
    disabled: bool,
) {
    panic_guard::guard(
        "adele_core_set_mcp_builtin_disabled",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let name = unsafe { cstr_to_string(name) };
            set_mcp_builtin_disabled_impl(core, name, disabled);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_set_mcp_builtin_disabled`].
/// `name_len` is the number of bytes at `name` -- see [`cstr_n_to_string`] for
/// the exact decode semantics (null/zero-length -> empty, embedded NUL kept,
/// invalid UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]. `name` must be null
/// (with any length), or point to at least `name_len` readable bytes for the
/// duration of the call. A length longer than the pointer's true allocation
/// is the caller's error and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_mcp_builtin_disabled_n(
    core: *mut Core,
    name: *const c_char,
    name_len: usize,
    disabled: bool,
) {
    panic_guard::guard(
        "adele_core_set_mcp_builtin_disabled_n",
        move || {
            // SAFETY: contract above.
            let name = unsafe { cstr_n_to_string(name, name_len) };
            set_mcp_builtin_disabled_impl(core, name, disabled);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_upsert_mcp_client_server`] and
/// [`adele_core_upsert_mcp_client_server_n`]: queue the upsert intent.
fn upsert_mcp_client_server_impl(core: *mut Core, server_json: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::WriteMcpClientServer(ClientServerWrite::Upsert {
        server_json,
    }));
}

/// Add one **external client-run** MCP server to the shared `client-mcp.toml`,
/// or edit the one of the same name, for the surface declared via
/// [`adele_core_set_mcp_surface`].
///
/// `server_json` is a JSON object: `name` (required), `command` (required),
/// `args` (array of strings), `namespace` (string or null), `enabled` (bool,
/// default `true`). A field this core does not know is refused rather than
/// ignored, so a client cannot believe it configured something it did not -- an
/// HTTP endpoint, for instance, which a client-run server cannot have (there is
/// no client-side secret store to authenticate one with).
///
/// `enabled` sets both grains at once: the definition's own flag and this
/// surface's membership. Editing a server preserves what the form does not carry
/// (`env`, `env_secrets`, `inherit_env`, `description`).
///
/// The write goes through the core because `client-mcp.toml` is machine-wide:
/// every Adele client on the box reads the same one, and a second independent
/// writer would be a correctness hazard for all of them. A malformed file is
/// refused rather than overwritten.
///
/// Takes effect on the next [`adele_core_connect`] -- a running MCP host is fixed
/// at start. An `mcp_client_servers` view event follows either way (including on
/// failure, which also emits a `toast`), carrying the state on disk so the panel
/// never keeps an edit that did not land.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]; `server_json` must be
/// NULL or a valid NUL-terminated UTF-8 string, borrowed for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_upsert_mcp_client_server(
    core: *mut Core,
    server_json: *const c_char,
) {
    panic_guard::guard(
        "adele_core_upsert_mcp_client_server",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let server_json = unsafe { cstr_to_string(server_json) };
            upsert_mcp_client_server_impl(core, server_json);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_upsert_mcp_client_server`].
/// `server_json_len` is the number of bytes at `server_json` -- see
/// [`cstr_n_to_string`] for the exact decode semantics (null/zero-length ->
/// empty, embedded NUL kept, invalid UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]. `server_json` must
/// be null (with any length), or point to at least `server_json_len`
/// readable bytes for the duration of the call. A length longer than the
/// pointer's true allocation is the caller's error and is not detectable
/// here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_upsert_mcp_client_server_n(
    core: *mut Core,
    server_json: *const c_char,
    server_json_len: usize,
) {
    panic_guard::guard(
        "adele_core_upsert_mcp_client_server_n",
        move || {
            // SAFETY: contract above.
            let server_json = unsafe { cstr_n_to_string(server_json, server_json_len) };
            upsert_mcp_client_server_impl(core, server_json);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_remove_mcp_client_server`] and
/// [`adele_core_remove_mcp_client_server_n`]: queue the remove intent.
fn remove_mcp_client_server_impl(core: *mut Core, name: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::WriteMcpClientServer(ClientServerWrite::Remove {
        name,
    }));
}

/// Delete one external client-run MCP server from the shared `client-mcp.toml`.
///
/// The definition is machine-wide, so this removes it for **every** surface, not
/// only this client's -- to stop hosting a server here while other clients keep
/// it, use [`adele_core_set_mcp_client_server_enabled`] with `enabled = false`.
///
/// Removing a name that is not defined is refused (and toasted) rather than
/// silently accepted. The event and timing contract is
/// [`adele_core_upsert_mcp_client_server`]'s.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]; `name` must be NULL or a
/// valid NUL-terminated UTF-8 string, borrowed for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_remove_mcp_client_server(core: *mut Core, name: *const c_char) {
    panic_guard::guard(
        "adele_core_remove_mcp_client_server",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let name = unsafe { cstr_to_string(name) };
            remove_mcp_client_server_impl(core, name);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_remove_mcp_client_server`].
/// `name_len` is the number of bytes at `name` -- see [`cstr_n_to_string`] for
/// the exact decode semantics (null/zero-length -> empty, embedded NUL kept,
/// invalid UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]. `name` must be null
/// (with any length), or point to at least `name_len` readable bytes for the
/// duration of the call. A length longer than the pointer's true allocation
/// is the caller's error and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_remove_mcp_client_server_n(
    core: *mut Core,
    name: *const c_char,
    name_len: usize,
) {
    panic_guard::guard(
        "adele_core_remove_mcp_client_server_n",
        move || {
            // SAFETY: contract above.
            let name = unsafe { cstr_n_to_string(name, name_len) };
            remove_mcp_client_server_impl(core, name);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_set_mcp_client_server_enabled`] and
/// [`adele_core_set_mcp_client_server_enabled_n`]: queue the set-enabled
/// intent.
fn set_mcp_client_server_enabled_impl(core: *mut Core, name: String, enabled: bool) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::WriteMcpClientServer(
        ClientServerWrite::SetEnabled { name, enabled },
    ));
}

/// Turn one external client-run MCP server on or off **for this client's
/// surface**.
///
/// Asymmetric on purpose, so one surface's choice never disturbs another sharing
/// the file: turning it **on** joins `[surfaces.<surface>].enabled` and switches
/// the definition's own `enabled` flag on, so the server really is hosted here;
/// turning it **off** drops this surface's entry only, leaving the definition
/// enabled for every other surface that lists it.
///
/// A name that is not defined is refused (and toasted) in either direction,
/// rather than materializing a surface entry for a server that does not exist.
///
/// Turning **on** a definition that carries an HTTP endpoint is refused too, for
/// the reason [`adele_core_upsert_mcp_client_server`] refuses to write one: the
/// client MCP host spawns a command, and an HTTP definition has none, so the row
/// could only ever report a server that failed to start. Turning one off stays
/// allowed, so a definition already in this surface's list has a way out, and so
/// does removing it.
///
/// The event and timing contract is [`adele_core_upsert_mcp_client_server`]'s.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]; `name` must be NULL or a
/// valid NUL-terminated UTF-8 string, borrowed for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_mcp_client_server_enabled(
    core: *mut Core,
    name: *const c_char,
    enabled: bool,
) {
    panic_guard::guard(
        "adele_core_set_mcp_client_server_enabled",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let name = unsafe { cstr_to_string(name) };
            set_mcp_client_server_enabled_impl(core, name, enabled);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_set_mcp_client_server_enabled`].
/// `name_len` is the number of bytes at `name` -- see [`cstr_n_to_string`] for
/// the exact decode semantics (null/zero-length -> empty, embedded NUL kept,
/// invalid UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]. `name` must be null
/// (with any length), or point to at least `name_len` readable bytes for the
/// duration of the call. A length longer than the pointer's true allocation
/// is the caller's error and is not detectable here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_mcp_client_server_enabled_n(
    core: *mut Core,
    name: *const c_char,
    name_len: usize,
    enabled: bool,
) {
    panic_guard::guard(
        "adele_core_set_mcp_client_server_enabled_n",
        move || {
            // SAFETY: contract above.
            let name = unsafe { cstr_n_to_string(name, name_len) };
            set_mcp_client_server_enabled_impl(core, name, enabled);
        },
        || (),
    );
}

/// Shared logic behind [`adele_core_send_command`] and
/// [`adele_core_send_command_n`]: queue the send-command intent.
fn send_command_impl(core: *mut Core, request_id: String, command_json: String) {
    // SAFETY: contract on the two public entry points above -- `core` must be
    // a live handle from [`adele_core_new`] or null.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SendCommand {
        request_id,
        command_json,
    });
}

/// Send an arbitrary management command (an `api::Command` serialized as JSON)
/// over the connector. The `CommandResult` is delivered later as a
/// `command_result` view event carrying the same `request_id`, so the caller can
/// correlate the reply. This is the generic settings/management channel
/// (connections, purposes, knowledge base) beyond the typed chat intents.
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle; `request_id`/`command_json` must be null or
/// valid C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_send_command(
    core: *mut Core,
    request_id: *const c_char,
    command_json: *const c_char,
) {
    panic_guard::guard(
        "adele_core_send_command",
        move || {
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let request_id = unsafe { cstr_to_string(request_id) };
            // SAFETY: C caller guarantees pointer is valid NUL-terminated string
            // for the duration of this call.
            let command_json = unsafe { cstr_to_string(command_json) };
            send_command_impl(core, request_id, command_json);
        },
        || (),
    );
}

/// Length-carrying twin of [`adele_core_send_command`].
/// `request_id_len`/`command_json_len` are the number of bytes at
/// `request_id`/`command_json` -- see [`cstr_n_to_string`] for the exact
/// decode semantics (null/zero-length -> empty, embedded NUL kept, invalid
/// UTF-8 replaced lossily).
///
/// A caught panic returns having sent nothing, the same as a null `core`.
///
/// # Safety
/// `core` must be a live handle. `request_id`/`command_json` must each be
/// null (with any length), or point to at least their matching `_len`
/// readable bytes for the duration of the call. A length longer than the
/// pointer's true allocation is the caller's error and is not detectable
/// here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_send_command_n(
    core: *mut Core,
    request_id: *const c_char,
    request_id_len: usize,
    command_json: *const c_char,
    command_json_len: usize,
) {
    panic_guard::guard(
        "adele_core_send_command_n",
        move || {
            // SAFETY: contract above.
            let request_id = unsafe { cstr_n_to_string(request_id, request_id_len) };
            // SAFETY: contract above.
            let command_json = unsafe { cstr_n_to_string(command_json, command_json_len) };
            send_command_impl(core, request_id, command_json);
        },
        || (),
    );
}

/// The ABI version of this crate's `extern "C"` surface: every entry point,
/// its signature, and the layout of any struct that crosses the boundary.
///
/// A single monotonically increasing counter, not a major/minor pair: there
/// is no compatibility promise between versions, so a consumer must treat any
/// difference as a mismatch. Compare the value returned here against
/// `ADELE_CORE_ABI_VERSION` from the header you compiled against. Rebuild
/// your consumer against the current header on any mismatch, including a
/// lower runtime value; do not attempt partial compatibility.
///
/// This cannot panic today; it reads one constant. It still runs behind the
/// panic guard, because every entry point does. A caught panic returns `0`,
/// which is not a version this crate ever assigns -- see
/// [`ADELE_CORE_ABI_VERSION`] -- so a consumer reads `0` as "no usable
/// version" rather than mistaking it for a real one.
#[unsafe(no_mangle)]
pub extern "C" fn adele_core_abi_version() -> u32 {
    panic_guard::guard("adele_core_abi_version", || ADELE_CORE_ABI_VERSION, || 0)
}

/// The ABI version [`adele_core_abi_version`] returns, as a plain constant so
/// the header carries `#define ADELE_CORE_ABI_VERSION` for a consumer that
/// wants a compile-time value to compare against.
pub const ADELE_CORE_ABI_VERSION: u32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adele_core_new_returns_null_when_callback_is_null() {
        let core = adele_core_new(None, std::ptr::null_mut());
        assert!(core.is_null(), "a null callback must yield a null handle");
    }

    #[test]
    fn cstr_n_to_string_null_ptr_decodes_as_empty_regardless_of_len() {
        // SAFETY: null with a nonzero len is documented to decode as empty
        // without dereferencing the pointer.
        let decoded = unsafe { cstr_n_to_string(std::ptr::null(), 5) };
        assert_eq!(decoded, "");
    }

    #[test]
    fn cstr_n_to_string_zero_len_decodes_as_empty_without_touching_ptr() {
        // An obviously invalid, non-null pointer. If the zero-len early
        // return did not come before `ptr` is touched, dereferencing this
        // would segfault the test process instead of the assertion below
        // failing cleanly.
        let bogus_ptr = 0xdead_beef_usize as *const c_char;
        // SAFETY: len == 0 is documented to return before `ptr` is read, so
        // an invalid, never-dereferenced `ptr` is sound here.
        let decoded = unsafe { cstr_n_to_string(bogus_ptr, 0) };
        assert_eq!(decoded, "");
    }

    #[test]
    fn cstr_n_to_string_preserves_an_embedded_nul_instead_of_truncating_at_it() {
        let data = b"before\0after";
        // SAFETY: `data` is a live, in-bounds `[u8; 12]` for the call.
        let decoded = unsafe { cstr_n_to_string(data.as_ptr().cast(), data.len()) };
        assert_eq!(decoded, "before\0after");
    }

    #[test]
    fn cstr_n_to_string_replaces_invalid_utf8_lossily() {
        let data = [b'a', 0xff, b'b'];
        // SAFETY: `data` is a live, in-bounds `[u8; 3]` for the call.
        let decoded = unsafe { cstr_n_to_string(data.as_ptr().cast(), data.len()) };
        assert_eq!(decoded, "a\u{FFFD}b");
    }

    #[test]
    fn cstr_n_to_string_with_len_shorter_than_the_buffer_truncates_the_decoded_value() {
        let data = b"hello world";
        // SAFETY: `data` is a live, in-bounds `[u8; 11]`; `len` (5) is
        // within it.
        let decoded = unsafe { cstr_n_to_string(data.as_ptr().cast(), 5) };
        assert_eq!(decoded, "hello");
    }

    /// Proves the short-`len` truncation in the test above is not merely
    /// discarding extra decoded characters: it maps a guard page directly
    /// after the input bytes, so any read past `len` -- a NUL-terminator
    /// scan, for instance -- crosses into unmapped memory and faults the
    /// whole test process rather than this assertion failing cleanly.
    ///
    /// What this proves: `cstr_n_to_string` does not read a single byte
    /// past `len` bytes from `ptr`, on this platform, for this input. What
    /// it does not prove: behaviour for every possible input or platform,
    /// or that no unrelated code elsewhere over-reads -- this test only
    /// exercises this one function, with this one guard-page layout. It is
    /// an ordinary `cargo test` run, not a run under Miri or a sanitizer.
    #[test]
    fn cstr_n_to_string_with_a_short_len_never_reads_into_the_page_immediately_after_it() {
        let page_size = 4096usize;
        // SAFETY: requests a fresh, anonymous, private two-page mapping;
        // the result is checked against `MAP_FAILED` immediately below
        // before it is used for anything.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page_size * 2,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED, "mmap failed");

        // SAFETY: `base` is a live two-page mapping from the `mmap` above;
        // offsetting by exactly one page stays within that same
        // allocation, so this pointer arithmetic is in-bounds.
        let second_page = unsafe { base.cast::<u8>().add(page_size) };
        // SAFETY: `second_page` is the start of the mapping's second page,
        // still mapped read-write from the `mmap` above, so dropping its
        // protection to `PROT_NONE` is a valid `mprotect` target.
        let protect_result =
            unsafe { libc::mprotect(second_page.cast(), page_size, libc::PROT_NONE) };
        assert_eq!(protect_result, 0, "mprotect failed");

        // Place the payload at the very end of the first (still readable)
        // page, so it is immediately followed by the PROT_NONE page: any
        // read past `len` bytes from it faults right away.
        let payload = b"hello";
        // SAFETY: `page_size - payload.len()` stays within the mapping's
        // first page, so this offset is in-bounds of the same allocation.
        let payload_start = unsafe { base.cast::<u8>().add(page_size - payload.len()) };
        // SAFETY: `payload_start..+payload.len()` sits entirely within the
        // first page, which is mapped read-write.
        unsafe {
            std::ptr::copy_nonoverlapping(payload.as_ptr(), payload_start, payload.len());
        }

        // SAFETY: `payload_start` points at exactly `payload.len()`
        // readable bytes, immediately followed by an unmapped page; this is
        // exactly the contract `cstr_n_to_string` documents for a `len`
        // that does not exceed the true allocation.
        let decoded = unsafe { cstr_n_to_string(payload_start.cast(), payload.len()) };
        assert_eq!(decoded, "hello");

        // SAFETY: tears down exactly the mapping created above, nothing
        // else.
        unsafe {
            libc::munmap(base, page_size * 2);
        }
    }
}
