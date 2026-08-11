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

mod builtins;
mod client_mcp;
mod engine;
mod markdown;
mod view_event;

// The markdown surface is `no_mangle`, so the cdylib exports it either way;
// re-exporting keeps it reachable by path for the rlib consumers (the spec).
pub use markdown::{
    adele_core_markdown_height_handler_name, adele_core_markdown_set_content_function,
    adele_core_markdown_set_content_script, adele_core_render_markdown,
    adele_core_render_markdown_document, adele_core_string_free,
};

use std::ffi::{CStr, c_char, c_void};

use desktop_assistant_client_common::TransportMode;

use crate::client_mcp::ClientServerWrite;
use crate::engine::{Core, Intent, ViewSink};
use crate::view_event::adele_output_from_str;

/// Decode a borrowed C string into an owned `String`. `null` ⇒ empty; invalid
/// UTF-8 ⇒ lossily replaced — never panics.
///
/// # Safety
/// `ptr` must be null or point to a valid NUL-terminated C string that stays
/// valid for the duration of the call.
pub(crate) unsafe fn cstr_to_string(ptr: *const c_char) -> String {
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

/// Check out queued message `index` into the composer to edit it (up-arrow
/// recall / a chip's edit affordance). The text loads via a `composer_text` view
/// event; re-submitting reinserts it in place. An out-of-range index is ignored.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_edit_queued(core: *mut Core, index: usize) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::EditQueued(index));
}

/// Remove queued message `index` (a chip's x) without sending it. An
/// out-of-range index is ignored.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_remove_queued(core: *mut Core, index: usize) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::RemoveQueued(index));
}

/// Abandon an in-progress queued-message edit: the checked-out message returns
/// to the queue unchanged and the composer clears. A no-op when not editing.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_cancel_queued_edit(core: *mut Core) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::CancelQueuedEdit);
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

/// Stage (or clear) a per-message model override for the next send. Empty
/// `connection_id`/`model_id` clears it (inherit the default); `effort` is
/// `"low"`/`"medium"`/`"high"` or empty (no effort hint).
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
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SelectModel {
        connection_id: unsafe { cstr_to_string(connection_id) },
        model_id: unsafe { cstr_to_string(model_id) },
        effort: unsafe { cstr_to_string(effort) },
    });
}

/// Request cancellation of a background task by id.
///
/// # Safety
/// `core` must be a live handle; `task_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_cancel_task(core: *mut Core, task_id: *const c_char) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::CancelTask(unsafe { cstr_to_string(task_id) }));
}

/// Fetch a background task's log page; the result arrives later as a `task_logs`
/// view event.
///
/// # Safety
/// `core` must be a live handle; `task_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_fetch_task_logs(core: *mut Core, task_id: *const c_char) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::FetchTaskLogs(unsafe { cstr_to_string(task_id) }));
}

/// Stage an explicit WebSocket bearer token for the next [`adele_core_connect`]
/// (empty ⇒ clear). Used verbatim as the WS bearer credential, bypassing the
/// D-Bus / `/login` token minting — the path a client with no local token minter
/// (e.g. macOS, which has no D-Bus bridge) uses after obtaining a token
/// out-of-band from the daemon's `/login`. Ignored for non-WS transports.
///
/// # Safety
/// `core` must be a live handle; `jwt` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_ws_jwt(core: *mut Core, jwt: *const c_char) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SetWsJwt(unsafe { cstr_to_string(jwt) }));
}

/// Set whether basic device context (name, username, home dir, hostname,
/// timezone, OS) is shared with the assistant on the next [`adele_core_connect`]
/// (#549). `true` (the default) shares it so the assistant can personalize;
/// `false` opts out, sending no context field / header at all. Staged on the
/// core and applied when the next connect builds its `ConnectionConfig`, so a
/// change takes effect on the following (re)connect. This backs the KDE KCM
/// "Share device info with the assistant" checkbox.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_share_client_context(core: *mut Core, enabled: bool) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SetShareClientContext(enabled));
}

/// Declare which `client-mcp.toml` surface this client resolves its MCP servers
/// (and `disabled_builtins`) under — `"mac"`, `"kde"`, … Server *definitions* are
/// machine-wide; the surface is the per-client enable layer, so one set of
/// servers can be configured once and switched on per client.
///
/// Call this once before [`adele_core_connect`]; it is read when the connect
/// starts the client MCP host, so a later change applies on the next
/// (re)connect. A NULL or empty name is ignored and the core keeps its default
/// surface (`kde`), which is what adele-kde relies on by never calling this.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]; `surface` must be NULL
/// or a valid NUL-terminated UTF-8 string, borrowed for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_mcp_surface(core: *mut Core, surface: *const c_char) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SetMcpSurface(unsafe { cstr_to_string(surface) }));
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
/// with no `mcp-*` feature — adele-kde's — answers with an empty list, which is
/// the honest "none linked in" rather than a missing reply.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_request_mcp_builtins(core: *mut Core) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::RequestMcpBuiltins);
}

/// Ask for this client's external client-run MCP servers — the `client-mcp.toml`
/// servers the surface declared via [`adele_core_set_mcp_surface`] hosts on the
/// edge — and their status. The answer arrives as an `mcp_client_servers` view
/// event carrying, per server: `name`, `transport` (`stdio`/`http`), `status`,
/// `tool_count`, and `namespace` (or null).
///
/// The sibling of [`adele_core_request_mcp_builtins`], and like it answerable with
/// **no connection**: which external servers this machine defines, and which of
/// them this surface hosts, are both properties of `client-mcp.toml`, so a
/// settings panel can call this before the first connect.
///
/// The list covers every **defined** server, not only the hosted ones, so a panel
/// can show — and switch back on — a server this surface has turned off. A server
/// this surface does not host reports `disabled`; a hosted one reports `enabled`
/// with a `0` tool count until a connection starts the client MCP host, and
/// `running` (with its live tool count) or `error` after that. A machine that
/// defines no external servers answers with an empty list — the honest "none
/// configured".
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_request_mcp_client_servers(core: *mut Core) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::RequestMcpClientServers);
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
/// Takes effect on the next [`adele_core_connect`] — a running MCP host is fixed
/// at start. An `mcp_builtins` view event follows either way (including on
/// failure, which also emits a `toast`), carrying the pending state so the panel
/// stays honest in the meantime. A NULL or empty `name` is refused.
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
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SetMcpBuiltinDisabled {
        name: unsafe { cstr_to_string(name) },
        disabled,
    });
}

/// Add one **external client-run** MCP server to the shared `client-mcp.toml`,
/// or edit the one of the same name, for the surface declared via
/// [`adele_core_set_mcp_surface`].
///
/// `server_json` is a JSON object: `name` (required), `command` (required),
/// `args` (array of strings), `namespace` (string or null), `enabled` (bool,
/// default `true`). A field this core does not know is refused rather than
/// ignored, so a client cannot believe it configured something it did not — an
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
/// Takes effect on the next [`adele_core_connect`] — a running MCP host is fixed
/// at start. An `mcp_client_servers` view event follows either way (including on
/// failure, which also emits a `toast`), carrying the state on disk so the panel
/// never keeps an edit that did not land.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]; `server_json` must be
/// NULL or a valid NUL-terminated UTF-8 string, borrowed for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_upsert_mcp_client_server(
    core: *mut Core,
    server_json: *const c_char,
) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::WriteMcpClientServer(ClientServerWrite::Upsert {
        server_json: unsafe { cstr_to_string(server_json) },
    }));
}

/// Delete one external client-run MCP server from the shared `client-mcp.toml`.
///
/// The definition is machine-wide, so this removes it for **every** surface, not
/// only this client's — to stop hosting a server here while other clients keep
/// it, use [`adele_core_set_mcp_client_server_enabled`] with `enabled = false`.
///
/// Removing a name that is not defined is refused (and toasted) rather than
/// silently accepted. The event and timing contract is
/// [`adele_core_upsert_mcp_client_server`]'s.
///
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]; `name` must be NULL or a
/// valid NUL-terminated UTF-8 string, borrowed for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_remove_mcp_client_server(core: *mut Core, name: *const c_char) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::WriteMcpClientServer(ClientServerWrite::Remove {
        name: unsafe { cstr_to_string(name) },
    }));
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
/// # Safety
/// `core` must be a live handle from [`adele_core_new`]; `name` must be NULL or a
/// valid NUL-terminated UTF-8 string, borrowed for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adele_core_set_mcp_client_server_enabled(
    core: *mut Core,
    name: *const c_char,
    enabled: bool,
) {
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::WriteMcpClientServer(
        ClientServerWrite::SetEnabled {
            name: unsafe { cstr_to_string(name) },
            enabled,
        },
    ));
}

/// Send an arbitrary management command (an `api::Command` serialized as JSON)
/// over the connector. The `CommandResult` is delivered later as a
/// `command_result` view event carrying the same `request_id`, so the caller can
/// correlate the reply. This is the generic settings/management channel
/// (connections, purposes, knowledge base) beyond the typed chat intents.
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
    // SAFETY: contract above.
    let Some(core) = (unsafe { core.as_ref() }) else {
        return;
    };
    core.send_intent(Intent::SendCommand {
        request_id: unsafe { cstr_to_string(request_id) },
        command_json: unsafe { cstr_to_string(command_json) },
    });
}
