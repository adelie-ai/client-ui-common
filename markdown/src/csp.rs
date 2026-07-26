//! CSP source-expression helpers shared by the page templates.

use base64::Engine as _;
use sha2::{Digest, Sha256};

/// Compute the CSP `'sha256-<base64>'` source expression for an inline script
/// body, so `script-src` can pin exactly that script and nothing else.
///
/// Why hashing rather than `'unsafe-inline'`: the page also hosts *untrusted*
/// assistant markup. `'unsafe-inline'` would let any `<script>` that survived
/// sanitization run; a hash lets exactly one known-good body run and nothing
/// else — including a byte-for-byte copy with one character changed.
pub(crate) fn sha256_source(script_body: &str) -> String {
    let digest = Sha256::digest(script_body.as_bytes());
    let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
    format!("'sha256-{b64}'")
}
