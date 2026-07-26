//! Spec for the host-side JavaScript bridge.
//!
//! The page CSP constrains script *in* the page. It does not constrain script
//! the **host** evaluates: `WKWebView.evaluateJavaScript` and
//! `webkit_web_view_evaluate_javascript` run outside the policy by design, which
//! is exactly what lets a host stream a growing reply into an already-loaded
//! document without reloading it and re-pinning a hash.
//!
//! That makes the host's evaluation its own trust boundary, and the reply is
//! untrusted. Ordinary assistant prose already renders to a fragment containing
//! raw `"` and raw newlines, so a host that formats the call itself hands the
//! author of the reply a way out of the string literal and into a new statement
//! — past both the sanitizer and the pinned `script-src`. The crate therefore
//! emits the complete, already-escaped statement rather than a function name and
//! a hope.

use adele_markdown::{bubble, js, markdown_to_html};

/// Extract the sole argument of `adeleSetContent("…");`.
///
/// Panics unless the statement is exactly one call to the bridge function with
/// exactly one string-literal argument — which is the property under test as
/// much as the decoded value is.
fn sole_string_argument(statement: &str) -> String {
    let open = format!("{}(", bubble::SET_CONTENT_FUNCTION);
    let inner = statement
        .strip_prefix(&open)
        .and_then(|rest| rest.strip_suffix(");"))
        .unwrap_or_else(|| panic!("not a single `{open}…);` call: {statement:?}"));
    serde_json::from_str(inner)
        .unwrap_or_else(|e| panic!("argument {inner:?} is not one string literal: {e}"))
}

/// Inputs that classically break out of a JS string literal, plus every C0
/// control character. Lifted from adele-gtk's `js_safe_string` corpus so the
/// clients that used to each own an escaper now share one spec.
fn breakout_corpus() -> Vec<String> {
    let mut inputs: Vec<String> = Vec::new();

    for code in 0x00u32..=0x1F {
        let c = char::from_u32(code).expect("C0 code points are valid chars");
        inputs.push(c.to_string());
        inputs.push(format!("before{c}after"));
    }

    for s in [
        "\"",
        "'",
        "\\",
        "\\\"",
        "\\n",             // literal backslash-n, not a newline
        "\");alert(1);//", // closes the literal and starts a statement
        "</script>",       // HTML closer, for a host that inlines the statement
        "`${alert(1)}`",   // template-literal injection
        "\u{2028}",        // LINE SEPARATOR: legal in JSON, ends a JS line
        "\u{2029}",        // PARAGRAPH SEPARATOR: same
        "\u{FEFF}",        // BOM / zero-width no-break space
        "\0embedded\0null",
        "emoji 🦀 and accents éàü",
        "中文字符",
        "",
    ] {
        inputs.push(s.to_string());
    }

    inputs
}

// --- js::string_literal: the escaping contract ------------------------------

#[test]
fn string_literal_round_trips_every_control_char_quote_and_js_line_separator() {
    for input in breakout_corpus() {
        let encoded = js::string_literal(&input);
        let decoded: String = serde_json::from_str(&encoded).unwrap_or_else(|e| {
            panic!("string_literal({input:?}) -> {encoded:?} is not a string literal: {e}")
        });
        assert_eq!(
            decoded, input,
            "round-trip mismatch: {input:?} encoded as {encoded:?}"
        );
    }
}

#[test]
fn string_literal_emits_no_character_that_can_end_the_literal_or_the_line() {
    for input in breakout_corpus() {
        let encoded = js::string_literal(&input);
        // The only unescaped quotes are the literal's own delimiters.
        assert_eq!(
            encoded.matches('"').count(),
            2,
            "stray quote for {input:?}: {encoded:?}"
        );
        assert!(
            encoded.starts_with('"') && encoded.ends_with('"'),
            "{encoded:?}"
        );
        for bad in ['\n', '\r', '\u{2028}', '\u{2029}'] {
            assert!(
                !encoded.contains(bad),
                "line terminator {bad:?} survived for {input:?}: {encoded:?}"
            );
        }
        assert!(
            !encoded.chars().any(|c| (c as u32) < 0x20),
            "raw control char survived for {input:?}: {encoded:?}"
        );
    }
}

#[test]
fn string_literal_escapes_the_angle_bracket_so_it_is_inert_inside_a_script_element() {
    // A host that builds a `<script>` block rather than calling
    // `evaluateJavaScript` would otherwise let `</script>` in a reply close the
    // element. Escaping `<` costs nothing: the literal still decodes to the
    // same text.
    let encoded = js::string_literal("</script><script>alert(1)</script>");
    assert!(!encoded.contains('<'), "{encoded:?}");
    assert!(
        !encoded.to_ascii_lowercase().contains("</script"),
        "{encoded:?}"
    );
    let decoded: String = serde_json::from_str(&encoded).expect("still a string literal");
    assert_eq!(decoded, "</script><script>alert(1)</script>");
}

#[test]
fn string_literal_of_empty_input_is_an_empty_literal() {
    assert_eq!(js::string_literal(""), "\"\"");
}

#[test]
fn string_literal_has_no_interior_nul_so_the_c_abi_cannot_truncate_it() {
    let encoded = js::string_literal("before\0after");
    assert!(!encoded.contains('\0'), "{encoded:?}");
    std::ffi::CString::new(encoded).expect("output is a valid C string");
}

// --- bubble::set_content_script: the host contract --------------------------

#[test]
fn set_content_script_is_one_call_to_the_documented_bridge_function() {
    let script = bubble::set_content_script("hello");
    assert!(
        script.starts_with(&format!("{}(", bubble::SET_CONTENT_FUNCTION)),
        "{script}"
    );
    assert!(script.ends_with(");"), "{script}");
    assert_eq!(sole_string_argument(&script), markdown_to_html("hello"));
}

#[test]
fn a_reply_that_closes_the_js_string_literal_cannot_start_a_new_statement() {
    // Not an exotic payload: this is prose with a quote in it. The rendered
    // fragment genuinely carries the breakout sequence, which is why the
    // escaping has to live here rather than in each host's format string.
    let hostile = r#"He said "x");alert(document.cookie);//"#;
    let fragment = markdown_to_html(hostile);
    assert!(
        fragment.contains("\");alert(document.cookie);//"),
        "the fragment a host would interpolate: {fragment:?}"
    );

    let script = bubble::set_content_script(hostile);
    assert!(
        !script.contains("\");alert("),
        "breakout reached the statement: {script}"
    );
    assert_eq!(
        sole_string_argument(&script),
        fragment,
        "the payload must stay data: {script}"
    );
}

#[test]
fn set_content_script_emits_no_raw_newline_quote_or_control_character() {
    // Any reply with two paragraphs renders to a multi-line fragment, and a raw
    // newline ends a JS line as effectively as a quote does.
    let markdown = "one\n\ntwo\n\n<b>\"three\"</b>";
    assert!(
        markdown_to_html(markdown).contains('\n'),
        "the fragment really is multi-line"
    );

    let script = bubble::set_content_script(markdown);
    assert!(!script.contains('\n'), "{script}");
    assert!(!script.contains('\r'), "{script}");
    assert_eq!(
        script.matches('"').count(),
        2,
        "only the literal's own delimiters: {script}"
    );
    assert!(
        !script.chars().any(|c| (c as u32) < 0x20),
        "raw control char in {script}"
    );
}

#[test]
fn set_content_script_sanitizes_before_it_escapes() {
    // The escaping is the second layer, not a replacement for the first: what
    // gets escaped is already-sanitized HTML.
    let script = bubble::set_content_script("<img src=x onerror=alert(1)>ok");
    let html = sole_string_argument(&script);
    assert!(!html.to_ascii_lowercase().contains("onerror"), "{html}");
    assert!(!html.contains("alert(1)"), "{html}");
    assert!(html.contains("ok"), "{html}");
}

#[test]
fn set_content_script_of_an_empty_reply_is_still_a_valid_call() {
    let script = bubble::set_content_script("");
    assert_eq!(sole_string_argument(&script), "");
}

#[test]
fn set_content_script_streams_exactly_what_the_initial_document_would_have_shown() {
    // The load-once/patch-in-place pair must agree, or a reply changes
    // appearance the moment it stops streaming.
    let markdown = "# Hi\n\n- [x] done\n\n| a |\n|---|\n| 1 |";
    let script = bubble::set_content_script(markdown);
    let document = bubble::document(markdown);
    assert!(
        document.contains(&sole_string_argument(&script)),
        "streamed fragment is not what the document embeds: {script}"
    );
}

#[test]
fn set_content_script_output_is_a_valid_c_string_for_the_ffi_boundary() {
    // The C ABI returns this through `CString::new`, which fails on an interior
    // NUL and would hand the host an empty bubble instead of the reply.
    for input in breakout_corpus() {
        let script = bubble::set_content_script(&input);
        std::ffi::CString::new(script.clone())
            .unwrap_or_else(|_| panic!("interior NUL for {input:?}: {script:?}"));
    }
}
