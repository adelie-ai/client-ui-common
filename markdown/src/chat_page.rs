//! The full-transcript chat page rendered by WebKitGTK in the GTK client.
//!
//! One document holds the whole conversation; the host swaps the transcript in
//! via `updateMessages(html)` and appends streaming text via `appendChunk`.
//! Contrast [`crate::bubble`], which is one document per message.

use std::sync::OnceLock;

use crate::csp::sha256_source;

/// Inline JavaScript body that powers the chat WebView.
///
/// The bytes here are hashed at startup and pinned via CSP `script-src
/// 'sha256-...'` so the WebView refuses to execute anything else — see
/// [`html_template`] and issue #25. Editing this string changes the hash;
/// the `csp_script_hash_matches_inline_script_body` test will catch drift.
const INLINE_SCRIPT: &str = r#"
function updateMessages(html) {
    document.getElementById('messages').innerHTML = html;
    scrollToBottom();
}

function appendChunk(text) {
    // Find streaming message or create one
    let streaming = document.querySelector('.streaming .content');
    if (!streaming) {
        let div = document.createElement('div');
        div.className = 'message assistant-message streaming';
        // Re-use the Adele avatar from the last assistant message, or use fallback
        let existingAvatar = document.querySelector('.assistant-message .avatar');
        let avatarHtml = existingAvatar
            ? existingAvatar.outerHTML
            : '<div class="avatar avatar-fallback">A</div>';
        div.innerHTML = avatarHtml + '<div class="bubble"><div class="label">Adele</div><div class="content"></div></div>';
        document.getElementById('messages').appendChild(div);
        streaming = div.querySelector('.content');
    }
    // Append raw text (for streaming, we accumulate and re-render on complete)
    streaming.textContent += text;
    scrollToBottom();
}

function setStatus(message) {
    let el = document.getElementById('status-indicator');
    document.getElementById('status-text').textContent = message;
    el.classList.add('visible');
    scrollToBottom();
}

function clearStatus() {
    document.getElementById('status-indicator').classList.remove('visible');
}

function scrollToBottom() {
    window.scrollTo(0, document.body.scrollHeight);
}
"#;

/// Compute the CSP `'sha256-...'` source expression for the inline script
/// body. Cached after the first call so callers keep cheap `&'static str`
/// semantics.
fn inline_script_csp_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| sha256_source(INLINE_SCRIPT))
}

/// Full HTML page template with embedded CSS.
///
/// CSP `script-src` is locked to the SHA-256 hash of [`INLINE_SCRIPT`] — no
/// `'unsafe-inline'`, no `'unsafe-eval'`, no remote scripts. Combined with
/// the raw-HTML stripping in [`crate::markdown_to_html`], a hostile assistant
/// message cannot execute JavaScript in the chat WebView. See gtk issue #25.
pub fn html_template() -> &'static str {
    static TEMPLATE: OnceLock<String> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let script_hash = inline_script_csp_hash();
        format!(
            r##"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data: file:; script-src {script_hash};">
<style>
* {{ margin: 0; padding: 0; box-sizing: border-box; }}

body {{
    background: #1a1d2e;
    color: #e0e0e0;
    font-family: system-ui, -apple-system, sans-serif;
    font-size: 14px;
    line-height: 1.6;
    padding: 16px;
}}

#messages {{
    display: flex;
    flex-direction: column;
    gap: 16px;
}}

.message {{
    display: flex;
    align-items: flex-start;
    gap: 10px;
}}

.avatar {{
    width: 28px;
    height: 28px;
    min-width: 28px;
    border-radius: 50%;
    object-fit: cover;
    object-position: center 15%;
    margin-top: 2px;
}}

.avatar-fallback {{
    background: #3a3f5c;
    color: #9ca3af;
    display: flex;
    align-items: center;
    justify-content: center;
    font-weight: 600;
    font-size: 13px;
}}

.bubble {{
    flex: 1;
    min-width: 0;
    border-radius: 8px;
    padding: 12px 16px;
}}

.user-message .bubble {{
    background: rgba(255, 189, 89, 0.08);
    border-left: 3px solid #ffbd59;
}}

.user-message .label {{
    color: #ffbd59;
    font-weight: 600;
    margin-bottom: 4px;
}}

.assistant-message .bubble {{
    background: rgba(92, 206, 154, 0.08);
    border-left: 3px solid #5cce9a;
}}

.assistant-message .label {{
    color: #5cce9a;
    font-weight: 600;
    margin-bottom: 4px;
}}

.assistant-message.streaming .bubble {{
    border-left-color: #84dac1;
}}

.assistant-message.streaming .label {{
    color: #84dac1;
}}

.content p {{ margin: 0.5em 0; }}
.content p:first-child {{ margin-top: 0; }}
.content p:last-child {{ margin-bottom: 0; }}

.content pre {{
    background: #232740;
    border-radius: 6px;
    padding: 12px;
    overflow-x: auto;
    margin: 0.5em 0;
}}

.content code {{
    font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
    font-size: 13px;
}}

.content :not(pre) > code {{
    background: #232740;
    padding: 2px 6px;
    border-radius: 3px;
}}

.content ul, .content ol {{
    padding-left: 1.5em;
    margin: 0.5em 0;
}}

.content table {{
    border-collapse: collapse;
    margin: 0.5em 0;
}}

.content th, .content td {{
    border: 1px solid #3a3f5c;
    padding: 6px 12px;
}}

.content th {{
    background: #232740;
}}

.content a {{
    color: #7aa3ff;
    text-decoration: none;
}}

.content a:hover {{
    text-decoration: underline;
}}

.cursor {{
    color: #84dac1;
    animation: blink 1s step-end infinite;
}}

@keyframes blink {{
    50% {{ opacity: 0; }}
}}

#status-indicator {{
    display: none;
    padding: 8px 16px;
    color: #9ca3af;
    font-size: 13px;
    font-style: italic;
}}

#status-indicator.visible {{
    display: flex;
    align-items: center;
    gap: 8px;
}}

#status-indicator .dot {{
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: #84dac1;
    animation: pulse 1.5s ease-in-out infinite;
}}

@keyframes pulse {{
    0%, 100% {{ opacity: 0.4; }}
    50% {{ opacity: 1; }}
}}

/* Light theme. WebKitGTK resolves `prefers-color-scheme` from the system color
   scheme (the `org.freedesktop.appearance color-scheme` portal), so this block
   applies whenever the desktop is not in dark mode. The
   dark palette above remains the default; these rules override only the
   colour-bearing properties so chat content stays legible and on-brand in
   light mode. Mirrors the GTK light palette in `style-light.css`:
   bg #1a1d2e->#ffffff, fg #e0e0e0->#1a1d2e, surface #232740->#f0f2f7,
   border #3a3f5c->#cdd3e0, user accent #ffbd59->#9a6b00,
   assistant accent #5cce9a->#178a6e, link #7aa3ff->#2456c8. */
@media (prefers-color-scheme: light) {{
    body {{
        background: #ffffff;
        color: #1a1d2e;
    }}

    .avatar-fallback {{
        background: #d6dae6;
        color: #555c6b;
    }}

    .user-message .bubble {{
        background: rgba(154, 107, 0, 0.07);
        border-left-color: #9a6b00;
    }}

    .user-message .label {{
        color: #9a6b00;
    }}

    .assistant-message .bubble {{
        background: rgba(23, 138, 110, 0.07);
        border-left-color: #178a6e;
    }}

    .assistant-message .label {{
        color: #178a6e;
    }}

    .assistant-message.streaming .bubble {{
        border-left-color: #1f9e7c;
    }}

    .assistant-message.streaming .label {{
        color: #1f9e7c;
    }}

    .content pre {{
        background: #f0f2f7;
    }}

    .content :not(pre) > code {{
        background: #f0f2f7;
    }}

    .content th, .content td {{
        border: 1px solid #cdd3e0;
    }}

    .content th {{
        background: #f0f2f7;
    }}

    .content a {{
        color: #2456c8;
    }}

    .cursor {{
        color: #1f9e7c;
    }}

    #status-indicator {{
        color: #555c6b;
    }}

    #status-indicator .dot {{
        background: #1f9e7c;
    }}
}}
</style>
</head>
<body>
<div id="messages"></div>
<div id="status-indicator"><span class="dot"></span><span id="status-text"></span></div>
<script>{INLINE_SCRIPT}</script>
</body>
</html>"##
        )
    })
}
