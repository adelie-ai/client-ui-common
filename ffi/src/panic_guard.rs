//! The panic barrier every `extern "C"` entry point in this crate runs its
//! body behind.
//!
//! A Rust panic must never unwind into a C caller: on current panic
//! semantics that aborts the whole process, in a caller's own address space,
//! with no chance to log why. [`guard`] runs a closure inside
//! `catch_unwind`, and on a caught panic logs the payload and the panic's
//! source location, then returns a caller-supplied neutral value instead.
//!
//! This does not, and cannot, catch every crash. Allocator exhaustion calls
//! `handle_alloc_error`, which aborts directly and never unwinds, so
//! `catch_unwind` never sees it.

use std::any::Any;
use std::cell::RefCell;
use std::panic::{self, AssertUnwindSafe, UnwindSafe};
use std::sync::Once;

thread_local! {
    /// The source location of the most recent panic caught on this thread.
    ///
    /// Set by the chained panic hook (installed once by [`install_hook`]) and
    /// read immediately after `catch_unwind` returns `Err` in [`guard`].
    /// `catch_unwind` runs synchronously on the calling thread, and the panic
    /// hook runs synchronously during unwinding, strictly before
    /// `catch_unwind` returns, so the write always happens before the read
    /// that follows it and there is no race.
    static LAST_PANIC_LOCATION: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Guards installation of the panic hook, so it runs once per process no
/// matter how many entry points call [`guard`].
static INSTALL_HOOK: Once = Once::new();

/// Install the location-capturing panic hook, once per process.
///
/// Chains onto whatever hook is already installed -- the default hook, on a
/// fresh process -- rather than replacing it, so the default hook's stderr
/// output (and backtrace, under `RUST_BACKTRACE`) still prints for local
/// development. `catch_unwind`'s payload never carries a source location;
/// this hook is the only way to recover one.
fn install_hook() {
    INSTALL_HOOK.call_once(|| {
        #[cfg(test)]
        ensure_a_tracing_subscriber_exists_for_tests();

        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let location = info.location().map(ToString::to_string);
            LAST_PANIC_LOCATION.with(|cell| *cell.borrow_mut() = location);
            previous(info);
        }));
    });
}

/// Make sure a real tracing subscriber -- not the process's built-in no-op
/// default -- is in place before this test binary's very first panic
/// reaches [`log_panic`].
///
/// `tracing` resolves and caches whether a given `tracing::error!` callsite
/// is worth dispatching the first time that callsite actually fires,
/// globally, once, for the life of the process. If that first fire happens
/// on a thread with no subscriber active at all (which any test that calls
/// [`guard`] without wrapping it in a capturing subscriber can trigger,
/// e.g. the panic-payload-shape tests below), the callsite can be judged
/// "nothing is interested" and skipped on every later dispatch -- even one
/// made through a subsequent test's own `tracing::subscriber::with_default`
/// scope, since the cached verdict short-circuits before the current
/// dispatcher is even consulted. `cargo test` runs this binary's tests in
/// parallel by default, so which test's panic is first to reach a given
/// callsite is not something a test can control.
///
/// Installing a permissive global default up front, before any test can
/// race to be first, means every callsite's interest is resolved against a
/// real subscriber from the start, so a later `with_default` scope (used by
/// the two payload-logging tests) keeps seeing events reliably regardless
/// of test execution order. This has no effect on the released cdylib:
/// `install_hook` calls this only `#[cfg(test)]`, and production callers
/// still install their own subscriber however the host process does.
#[cfg(test)]
fn ensure_a_tracing_subscriber_exists_for_tests() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
}

/// Run `f` behind a panic barrier.
///
/// On success, returns `f`'s value. If `f` panics, the panic is logged with
/// `label` (which entry point it happened in), its payload, and its source
/// location, and `fallback()` supplies the return value in place of letting
/// the panic unwind further.
///
/// `fallback` is a closure rather than a plain value so a fallback that owns
/// memory (a caller-owned C string, for instance) is allocated only when a
/// panic actually happened, never on every ordinary call.
///
/// No caller needs [`std::panic::AssertUnwindSafe`]. Every argument these
/// entry points take is a raw pointer, a `bool`, a `usize`, or an
/// `Option<extern "C" fn(..)>`, all unconditionally [`UnwindSafe`] -- a
/// property of the current parameter types, not a guarantee this function
/// enforces. A future entry point that captures a `&mut` reference across
/// this boundary will fail to compile here rather than silently widen the
/// unsafety; reach for a narrower fix, such as converting the reference to a
/// raw pointer before the closure, rather than `AssertUnwindSafe`.
pub(crate) fn guard<F, R>(label: &str, f: F, fallback: impl FnOnce() -> R) -> R
where
    F: FnOnce() -> R + UnwindSafe,
{
    install_hook();
    match panic::catch_unwind(f) {
        Ok(value) => value,
        Err(payload) => {
            let location = LAST_PANIC_LOCATION
                .with(|cell| cell.borrow_mut().take())
                .unwrap_or_else(|| "unknown location".to_string());
            // `&*payload` (not `&payload`) so the trait object keeps the
            // vtable of the ORIGINAL panic value. A reference to the `Box`
            // itself would erase to `Box<dyn Any + Send>` as the concrete
            // type, and every downcast below would fail.
            log_panic(label, &location, &*payload);
            fallback()
        }
    }
}

/// Run `f` behind the same panic barrier [`guard`] uses, for a caller whose
/// closure carries a `&mut` reference across the boundary.
///
/// [`guard`]'s own doc explains why it requires `F: UnwindSafe` and never
/// accepts [`AssertUnwindSafe`] itself: every argument an `extern "C"` entry
/// point takes is already unconditionally [`UnwindSafe`], so asserting it
/// would hide a real future mistake instead of describing an existing one.
/// The FFI actor's per-message loop (issue #90) is a different situation.
/// `Engine::handle_intent` and `Engine::dispatch` need `&mut Engine` to
/// cross the boundary, and a `&mut` reference is excluded from
/// [`UnwindSafe`] on purpose: a panic partway through a mutation can leave
/// the referent holding a partial change. That risk is real here, not a
/// formality — a caught panic on the actor does not undo whatever the
/// message was in the middle of changing. The caller states that
/// explicitly by wrapping its own closure in [`AssertUnwindSafe`] and
/// passing it here; this function contributes only the logging [`guard`]
/// already does, so both boundaries are logged the same way.
///
/// Returns `Ok(value)` on success, or `Err(())` after logging on a caught
/// panic. Unlike [`guard`], there is no single fallback value that fits
/// every message shape, so the caller decides what happens next.
pub(crate) fn guard_actor_step<F, R>(label: &str, f: AssertUnwindSafe<F>) -> Result<R, ()>
where
    F: FnOnce() -> R,
{
    install_hook();
    match panic::catch_unwind(f) {
        Ok(value) => Ok(value),
        Err(payload) => {
            let location = LAST_PANIC_LOCATION
                .with(|cell| cell.borrow_mut().take())
                .unwrap_or_else(|| "unknown location".to_string());
            log_panic(label, &location, &*payload);
            Err(())
        }
    }
}

/// Log a caught panic's payload and location.
///
/// The payload is downcast to `&str` and `String`, the two types `panic!`
/// itself produces, and logged either way; anything else is logged as a
/// non-string payload rather than silently dropped.
fn log_panic(label: &str, location: &str, payload: &(dyn Any + Send)) {
    if let Some(message) = payload.downcast_ref::<&str>() {
        tracing::error!(
            entry_point = label,
            location,
            payload = *message,
            "panic caught at the C ABI boundary"
        );
    } else if let Some(message) = payload.downcast_ref::<String>() {
        tracing::error!(
            entry_point = label,
            location,
            payload = message.as_str(),
            "panic caught at the C ABI boundary"
        );
    } else {
        tracing::error!(
            entry_point = label,
            location,
            "panic caught at the C ABI boundary with a non-string payload"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, CString, c_char};
    use std::io;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    /// A tracing writer that appends every formatted log line into a shared
    /// buffer, so a test can assert on what [`guard`] logged.
    #[derive(Clone, Default)]
    struct CaptureBuffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for CaptureBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CaptureBuffer {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `f` under a tracing subscriber scoped to this call, and return
    /// `f`'s result alongside everything it logged, formatted as text.
    fn capture_tracing_output<T>(f: impl FnOnce() -> T) -> (T, String) {
        let buffer = CaptureBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        let result = tracing::subscriber::with_default(subscriber, f);
        let bytes = buffer
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        (result, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[test]
    fn guard_returns_the_closures_value_when_it_does_not_panic() {
        let value = guard("no-panic", || 42, || 0);
        assert_eq!(value, 42);
    }

    #[test]
    fn guard_returns_the_void_fallback_when_the_body_panics() {
        let value: () = guard("void-shape", || panic!("void body panics"), || ());
        assert_eq!(value, ());
    }

    #[test]
    fn guard_returns_the_null_handle_fallback_when_the_body_panics() {
        let value: *mut u8 = guard(
            "handle-shape",
            || -> *mut u8 { panic!("handle body panics") },
            std::ptr::null_mut::<u8>,
        );
        assert!(value.is_null());
    }

    #[test]
    fn guard_returns_a_fresh_caller_owned_string_fallback_when_the_body_panics() {
        let ptr = guard(
            "caller-owned-string-shape",
            || -> *mut c_char { panic!("string body panics") },
            || {
                CString::new("fallback")
                    .expect("no interior NUL")
                    .into_raw()
            },
        );
        assert!(!ptr.is_null());
        // SAFETY: `ptr` came from `CString::into_raw` in the fallback closure
        // above, has not been freed, and this call reclaims it exactly once.
        let recovered = unsafe { CString::from_raw(ptr) };
        assert_eq!(recovered.to_str().expect("ASCII fallback"), "fallback");
    }

    #[test]
    fn guard_returns_a_distinct_allocation_on_each_caller_owned_string_fallback() {
        let make = || {
            guard(
                "caller-owned-string-shape",
                || -> *mut c_char { panic!("string body panics") },
                || {
                    CString::new("fallback")
                        .expect("no interior NUL")
                        .into_raw()
                },
            )
        };
        let first = make();
        let second = make();
        assert_ne!(
            first, second,
            "each caller-owned fallback must be its own allocation, since the \
             caller frees each pointer independently"
        );
        // SAFETY: each pointer came from its own `CString::into_raw` call
        // above and is freed here exactly once.
        unsafe {
            drop(CString::from_raw(first));
            drop(CString::from_raw(second));
        }
    }

    #[test]
    fn guard_returns_the_static_string_fallback_when_the_body_panics() {
        static FALLBACK: &CStr = c"fallback";
        let ptr: *const c_char = guard(
            "static-string-shape",
            || -> *const c_char { panic!("static body panics") },
            || FALLBACK.as_ptr(),
        );
        assert_eq!(ptr, FALLBACK.as_ptr());
    }

    #[test]
    fn guard_logs_a_str_panic_payload_with_its_location() {
        let (_, output) = capture_tracing_output(|| {
            guard::<_, ()>("str-payload-entry-point", || panic!("boom"), || ())
        });
        assert!(
            output.contains("str-payload-entry-point"),
            "log did not name the entry point: {output}"
        );
        assert!(
            output.contains("boom"),
            "log did not carry the &str payload: {output}"
        );
        assert!(
            output.contains("panic_guard.rs"),
            "log did not carry a source location: {output}"
        );
    }

    #[test]
    fn guard_logs_a_string_panic_payload_with_its_location() {
        let owned = String::from("boom-owned");
        let (_, output) = capture_tracing_output(|| {
            guard::<_, ()>(
                "string-payload-entry-point",
                move || panic!("{owned}"),
                || (),
            )
        });
        assert!(
            output.contains("string-payload-entry-point"),
            "log did not name the entry point: {output}"
        );
        assert!(
            output.contains("boom-owned"),
            "log did not carry the String payload: {output}"
        );
        assert!(
            output.contains("panic_guard.rs"),
            "log did not carry a source location: {output}"
        );
    }
}
