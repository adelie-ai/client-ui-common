//! Enforces, at test time, that every C ABI entry point runs its body
//! behind the panic guard.
//!
//! `lib.rs`'s and `markdown.rs`'s module docs both claim every `extern "C"`
//! entry point calls `panic_guard::guard`. Nothing else holds them to that:
//! a future entry point that forgets to call it compiles and links cleanly,
//! and only fails the day a real panic reaches it in the field. This module
//! reads both files' own source at compile time (`include_str!`) and checks
//! the claim mechanically, so a missing guard fails the test suite instead.
//!
//! This is a text scan, not a full parser, but it is careful about the
//! things that would otherwise fool a naive brace count: string and char
//! literals, raw strings, and both comment forms. Getting this wrong is
//! worse than not having the test at all -- either failure mode (missing a
//! real gap, or flagging a guarded function as unguarded) makes the check
//! worthless or actively misleading. This is cfg(test)-only in its entirety
//! and never compiles into the released cdylib.

#![cfg(test)]

/// One `pub extern "C" fn` / `pub unsafe extern "C" fn` found in a source
/// file, with its body's exact source span already isolated.
struct EntryPoint {
    name: String,
    /// The body span (`{ ... }`, inclusive), read from the *masked* source
    /// (see [`mask_non_code`]) so a search over it only ever matches real
    /// code, never a mention inside a comment or string.
    masked_body: String,
}

/// Copy `source`, replacing every byte that sits inside a line comment, a
/// (nesting-aware) block comment, a string literal, a raw string literal
/// (`r"..."` / `r#"...#"#` / ... with any number of `#`), or a char literal
/// with an ASCII space. Everything else -- keywords, identifiers, real
/// braces and parens -- is copied verbatim, at the same byte offsets as
/// `source`, so a `{`, `}`, or `fn` that only appears inside one of those
/// constructs can no longer be mistaken for real code.
fn mask_non_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out: Vec<u8> = vec![b' '; bytes.len()];
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];

        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth = 1usize;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }

        if b == b'r' && matches!(bytes.get(i + 1), Some(b'"') | Some(b'#')) {
            let mut j = i + 1;
            let mut hashes = 0usize;
            while bytes.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if bytes.get(j) == Some(&b'"') {
                out[i] = b'r';
                j += 1;
                loop {
                    if j >= bytes.len() {
                        break;
                    }
                    if bytes[j] == b'"' {
                        let mut k = j + 1;
                        let mut closing_hashes = 0usize;
                        while closing_hashes < hashes && bytes.get(k) == Some(&b'#') {
                            closing_hashes += 1;
                            k += 1;
                        }
                        if closing_hashes == hashes {
                            j = k;
                            break;
                        }
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
            // `r` followed by neither `"` nor a run of `#` into `"` is just
            // the identifier `r`; fall through to ordinary code below.
        }

        if b == b'"' {
            out[i] = b'"';
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    out[i] = b'"';
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        if b == b'\'' {
            // A char literal (`'a'`, `'\n'`, `'\''`) and a lifetime
            // (`'static`, `'a`) both start with `'`. Only a char literal is
            // followed by a closing `'` at the expected offset; mask only
            // that case, so a lifetime's identifier stays ordinary code.
            let escaped = bytes.get(i + 1) == Some(&b'\\');
            let close_offset = if escaped { i + 3 } else { i + 2 };
            if bytes.get(close_offset) == Some(&b'\'') {
                out[i] = b'\'';
                out[close_offset] = b'\'';
                i = close_offset + 1;
                continue;
            }
        }

        out[i] = b;
        i += 1;
    }
    String::from_utf8(out).expect("masking only ever replaces bytes with the ASCII space")
}

/// Find every `pub extern "C" fn NAME(...)` / `pub unsafe extern "C" fn
/// NAME(...)` in `source` and return its name plus its body's masked span.
///
/// The marker itself is searched for in the **unmasked** source, not the
/// masked copy: `extern "C"` is Rust grammar, and the `"C"` in it is
/// lexically a string literal like any other, so [`mask_non_code`] blanks
/// its content -- masking cannot distinguish "a string used as a value"
/// from "a string used as an ABI specifier". Everything past the marker
/// (the parameter list and the body) is still read from the masked copy,
/// so brace and paren nesting stay safe from a string or comment inside
/// them.
fn find_entry_points(source: &str) -> Vec<EntryPoint> {
    let masked = mask_non_code(source);
    let masked_bytes = masked.as_bytes();
    let mut entry_points = Vec::new();

    for marker in ["pub extern \"C\" fn ", "pub unsafe extern \"C\" fn "] {
        let mut search_from = 0;
        while let Some(rel) = source[search_from..].find(marker) {
            let sig_start = search_from + rel;
            let name_start = sig_start + marker.len();
            let name_end = masked[name_start..]
                .find(['(', ' '])
                .map(|n| name_start + n)
                .unwrap_or_else(|| {
                    panic!("no parameter list after `{marker}` at byte {sig_start}")
                });
            let name = masked[name_start..name_end].to_string();

            let paren_open = masked[name_end..]
                .find('(')
                .map(|n| name_end + n)
                .unwrap_or_else(|| panic!("{name}: no parameter list found"));
            let mut depth = 0i32;
            let mut idx = paren_open;
            loop {
                match masked_bytes[idx] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            idx += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                idx += 1;
                assert!(
                    idx < masked_bytes.len(),
                    "{name}: unterminated parameter list"
                );
            }

            let body_start = masked[idx..]
                .find('{')
                .map(|n| idx + n)
                .unwrap_or_else(|| panic!("{name}: no body opening brace found"));

            let mut depth = 0i32;
            let mut k = body_start;
            let body_end = loop {
                match masked_bytes[k] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break k;
                        }
                    }
                    _ => {}
                }
                k += 1;
                assert!(k < masked_bytes.len(), "{name}: unterminated body");
            };

            entry_points.push(EntryPoint {
                name,
                masked_body: masked[body_start..=body_end].to_string(),
            });
            search_from = body_end + 1;
        }
    }

    entry_points
}

/// A floor on how many `extern "C"` entry points across `lib.rs` and
/// `markdown.rs` this scanner should find: comfortably below the 34 counted
/// when this test was written (27 in `lib.rs`, the ABI-version accessor
/// #86 added, and 6 in `markdown.rs`), so ordinary growth of the ABI
/// surface never fails this test.
///
/// This proves only that the scanner found a plausible number of entry
/// points and so has not silently stopped matching -- it does not, and is
/// not meant to, detect or police growth of the surface. That is
/// `scripts/check-abi-bump.sh`'s job (added by #86), which fails a header
/// change with no version bump. Pinning this to an exact count would make
/// every legitimate new entry point fail a test whose only fix is editing
/// the number, which teaches editing a test to make it green rather than
/// reading why it failed.
const MINIMUM_ENTRY_POINT_COUNT: usize = 30;

#[test]
fn every_extern_c_entry_point_runs_behind_the_panic_guard() {
    let sources: [(&str, &str); 2] = [
        ("lib.rs", include_str!("lib.rs")),
        ("markdown.rs", include_str!("markdown.rs")),
    ];

    let mut unguarded = Vec::new();
    let mut checked = 0usize;
    for (file, source) in sources {
        let entry_points = find_entry_points(source);
        assert!(
            !entry_points.is_empty(),
            "{file}: found no `pub extern \"C\" fn` at all -- the scanner \
             itself is broken, not the file"
        );
        for entry_point in entry_points {
            checked += 1;
            if !entry_point.masked_body.contains("panic_guard::guard(") {
                unguarded.push(format!("{file}::{}", entry_point.name));
            }
        }
    }

    // Checked first, and reported by name: this is the actionable failure a
    // future author who forgets to wrap a new entry point should see.
    assert!(
        unguarded.is_empty(),
        "these C ABI entry points do not call panic_guard::guard: {unguarded:?}"
    );
    // Checked second, as a weaker sanity check on the scanner itself: a
    // count well below what this scanner has always found means it
    // stopped matching real signatures, not that the surface shrank.
    assert!(
        checked >= MINIMUM_ENTRY_POINT_COUNT,
        "found only {checked} C ABI entry points, expected at least \
         {MINIMUM_ENTRY_POINT_COUNT}; the scanner drifted from the real \
         signatures, so this run does not prove what it claims to"
    );
}

#[test]
fn mask_non_code_blanks_comments_and_string_and_char_literals_but_keeps_code() {
    let source = concat!(
        "fn demo() { // a comment with { and }\n",
        "    let s = \"a { brace } in a string\";\n",
        "    let raw = r#\"a } in a raw \" string\"#;\n",
        "    let c = '{';\n",
        "    let life: &'static str = \"x\";\n",
        "    /* a block /* nested */ comment { too } */\n",
        "    real_code_brace();\n",
        "}\n",
    );
    let masked = mask_non_code(source);

    assert!(!masked.contains("brace } in a string"));
    assert!(!masked.contains("a } in a raw"));
    assert!(!masked.contains("nested"));
    assert!(
        masked.contains("real_code_brace();"),
        "real code must survive masking"
    );
    assert!(
        masked.contains("'static"),
        "a lifetime is not a char literal and must survive masking"
    );

    // Masking must not change the byte length, so spans computed against it
    // still slice the original source correctly.
    assert_eq!(masked.len(), source.len());
}
