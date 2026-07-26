//! Encoding for the one place a rendered reply leaves this crate as *source*
//! rather than as data: JavaScript the **host** evaluates.
//!
//! # Why this is a trust boundary of its own
//!
//! The page templates pin their inline script with a SHA-256 CSP `script-src`,
//! which constrains script *in* the page. It does not constrain
//! `WKWebView.evaluateJavaScript` / `webkit_web_view_evaluate_javascript`: host
//! evaluation is exempt by design, and that exemption is what lets a client
//! stream a growing reply into an already-loaded document instead of reloading
//! it. So a string this crate hands a host to evaluate is script, and the reply
//! inside it is untrusted.
//!
//! Ordinary assistant prose is enough to break a naive `format!`: a reply
//! containing a double quote renders to a fragment containing a raw double
//! quote, and any two-paragraph reply renders to a fragment containing raw
//! newlines. Either ends the string literal it was interpolated into, and what
//! follows is executed. That is why callers get [`string_literal`] (and, for
//! the bubble page, a whole pre-built statement from
//! [`crate::bubble::set_content_script`]) instead of being trusted to quote.

/// Encode `value` as a complete, quoted JavaScript string literal.
///
/// The result is safe to interpolate into JavaScript source: it is delimited by
/// its own double quotes, and nothing inside can terminate the literal, the
/// line, or an enclosing `<script>` element.
///
/// Why JSON: a JSON string literal is a JavaScript string literal, and
/// `serde_json`'s encoder is a widely-audited implementation of that grammar —
/// a hand-rolled escaper on this boundary is exactly the kind of code that ends
/// up one case short. Three characters need handling on top of JSON, because
/// JSON permits them raw while JavaScript does not treat them as ordinary text:
///
/// - `U+2028` / `U+2029` are JavaScript *line terminators*, so a raw one ends
///   the statement even though it sits inside a valid JSON string.
/// - `<` is escaped so a caller that inlines the result into a `<script>`
///   element cannot have the element closed by `</script>` in a reply. It
///   decodes back to `<`, so the value the page sees is unchanged.
///
/// ```
/// let literal = adele_markdown::js::string_literal("say \"hi\"\n");
/// assert_eq!(literal, r#""say \"hi\"\n""#);
/// ```
pub fn string_literal(value: &str) -> String {
    // Serializing a `&str` cannot fail (no non-string map keys, no non-finite
    // floats, no custom `Serialize`), but failing closed to an empty literal
    // keeps the output a valid literal in every case rather than a `panic!` on
    // the render path.
    let json = serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""));

    // Safe as post-processing: `serde_json` emits `"`, `\`, and `\uXXXX` escapes
    // whose characters are all ASCII alphanumerics, backslash or quote, so
    // neither `<` nor a line separator can appear in the output except as
    // itself, carried over verbatim from `value`.
    json.replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
        .replace('<', "\\u003C")
}
