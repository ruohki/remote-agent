//! Embedded HTML/CSS/JS for the agent chat window (a WKWebView on macOS, WebView2 on
//! Windows). No build step: the page is one self-contained string with the operator name
//! substituted in. IPC contract:
//!
//! * JS → Rust: `window.__ipc(json)` posts `{type:"ready"}` / `{type:"send",text}` /
//!   `{type:"disconnect"}` (each platform wires `__ipc` to its native bridge).
//! * Rust → JS: `window.__agent.push({from,text,ts_ms})`, `window.__agent.setOperator(name)`,
//!   `window.__agent.setStatus(text)`, `window.__agent.setConnected(bool)`.

/// Build the chat page for `operator`. `operator` is inserted as a JS string literal
/// (JSON-escaped) so it cannot break out of the script.
pub fn chat_html(operator: &str) -> String {
    let op = serde_json::to_string(operator).unwrap_or_else(|_| "\"operator\"".into());
    HTML.replace("__OPERATOR__", &op)
}

const HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no">
<title>Remote support</title>
<style>
  :root {
    color-scheme: light dark;
    --bg: #f5f6f8;
    --panel: #ffffff;
    --header: #ffffff;
    --line: #e6e8ec;
    --text: #1f232b;
    --muted: #8a92a1;
    --bubble-in: #eceef2;
    --bubble-in-text: #1f232b;
    --accent: #2f6bff;
    --accent-text: #ffffff;
    --danger: #e5484d;
    --composer: #f0f1f4;
    --shadow: 0 1px 2px rgba(0,0,0,.06), 0 8px 24px rgba(0,0,0,.06);
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #14161a;
      --panel: #14161a;
      --header: #1b1e24;
      --line: #262a31;
      --text: #e9ecf1;
      --muted: #8b93a3;
      --bubble-in: #23272f;
      --bubble-in-text: #e9ecf1;
      --accent: #3f7bff;
      --accent-text: #ffffff;
      --danger: #ff6169;
      --composer: #1b1e24;
      --shadow: none;
    }
  }
  * { box-sizing: border-box; }
  html, body { height: 100%; margin: 0; }
  body {
    font: 14px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
    color: var(--text);
    background: var(--bg);
    display: flex; flex-direction: column;
    -webkit-user-select: none; user-select: none;
    overflow: hidden;
  }
  header {
    flex: 0 0 auto;
    display: flex; align-items: center; gap: 10px;
    padding: 10px 12px;
    background: var(--header);
    border-bottom: 1px solid var(--line);
    -webkit-app-region: drag;
  }
  header .avatar {
    width: 34px; height: 34px; border-radius: 50%;
    background: linear-gradient(135deg, #3f7bff, #6a5cff);
    color: #fff; font-weight: 600; font-size: 15px;
    display: grid; place-items: center; flex: 0 0 auto;
  }
  header .who { min-width: 0; flex: 1 1 auto; }
  header .name { font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  header .sub { color: var(--muted); font-size: 12px; display: flex; align-items: center; gap: 6px; }
  header .dot { width: 8px; height: 8px; border-radius: 50%; background: var(--muted); flex: 0 0 auto; }
  header .dot.on { background: #35c46a; box-shadow: 0 0 0 3px rgba(53,196,106,.18); }
  header .end {
    -webkit-app-region: no-drag;
    appearance: none; border: 1px solid transparent;
    background: color-mix(in srgb, var(--danger) 12%, transparent);
    color: var(--danger); font-weight: 600; font-size: 12px;
    padding: 6px 10px; border-radius: 8px; cursor: pointer; flex: 0 0 auto;
  }
  header .end:hover { background: color-mix(in srgb, var(--danger) 20%, transparent); }

  main {
    flex: 1 1 auto; overflow-y: auto; padding: 12px 12px 4px;
    display: flex; flex-direction: column; gap: 2px;
    scroll-behavior: smooth;
  }
  .empty {
    margin: auto; text-align: center; color: var(--muted); padding: 24px;
    display: flex; flex-direction: column; gap: 6px; align-items: center;
  }
  .empty .big { font-size: 15px; color: var(--text); font-weight: 600; }
  .day { align-self: center; color: var(--muted); font-size: 11px; margin: 12px 0 6px; }
  .row { display: flex; margin-top: 2px; }
  .row.first { margin-top: 10px; }
  .row.in { justify-content: flex-start; }
  .row.out { justify-content: flex-end; }
  .bubble {
    max-width: 78%; padding: 7px 11px; border-radius: 16px;
    white-space: pre-wrap; word-wrap: break-word; overflow-wrap: anywhere;
    -webkit-user-select: text; user-select: text;
    box-shadow: var(--shadow);
  }
  .in .bubble { background: var(--bubble-in); color: var(--bubble-in-text); border-bottom-left-radius: 5px; }
  .out .bubble { background: var(--accent); color: var(--accent-text); border-bottom-right-radius: 5px; }
  .row.grouped.in .bubble { border-top-left-radius: 5px; }
  .row.grouped.out .bubble { border-top-right-radius: 5px; }
  .time { font-size: 10px; color: var(--muted); margin: 2px 4px 0; }
  .row.in .time { text-align: left; }
  .row.out .time { text-align: right; }

  .pill {
    position: absolute; left: 50%; transform: translateX(-50%);
    bottom: 74px; background: var(--accent); color: #fff;
    font-size: 12px; font-weight: 600; padding: 6px 12px; border-radius: 999px;
    box-shadow: 0 4px 14px rgba(0,0,0,.25); cursor: pointer; display: none;
  }
  .pill.show { display: block; }

  footer {
    flex: 0 0 auto; padding: 8px 10px 10px; border-top: 1px solid var(--line);
    background: var(--header); display: flex; align-items: flex-end; gap: 8px;
  }
  #box {
    flex: 1 1 auto; resize: none; border: none; outline: none;
    background: var(--composer); color: var(--text);
    border-radius: 18px; padding: 9px 13px; font: inherit;
    max-height: 120px; min-height: 20px; -webkit-user-select: text; user-select: text;
  }
  #box::placeholder { color: var(--muted); }
  #send {
    flex: 0 0 auto; width: 34px; height: 34px; border-radius: 50%; border: none;
    background: var(--accent); color: #fff; cursor: pointer; display: grid; place-items: center;
    transition: opacity .12s;
  }
  #send:disabled { opacity: .4; cursor: default; }
  #send svg { width: 17px; height: 17px; }
</style>
</head>
<body>
  <header>
    <div class="avatar" id="avatar">?</div>
    <div class="who">
      <div class="name" id="opname">Operator</div>
      <div class="sub"><span class="dot" id="dot"></span><span id="status">connecting…</span></div>
    </div>
    <button class="end" id="end">End session</button>
  </header>
  <main id="log">
    <div class="empty" id="empty">
      <div class="big" id="emptyTitle">Say hi 👋</div>
      <div>Messages you send here are private to this session.</div>
    </div>
  </main>
  <div class="pill" id="pill">New messages ↓</div>
  <footer>
    <textarea id="box" rows="1" placeholder="Message…" autocomplete="off"></textarea>
    <button id="send" disabled aria-label="Send">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 2 11 13"/><path d="M22 2 15 22l-4-9-9-4 20-7z"/></svg>
    </button>
  </footer>
<script>
(function () {
  var OPERATOR = __OPERATOR__;
  var log = document.getElementById('log');
  var empty = document.getElementById('empty');
  var box = document.getElementById('box');
  var send = document.getElementById('send');
  var pill = document.getElementById('pill');
  var dot = document.getElementById('dot');
  var statusEl = document.getElementById('status');
  var lastFrom = null, lastRow = null, lastDay = null;

  function ipc(obj) {
    var s = JSON.stringify(obj);
    try {
      if (window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.agent) {
        window.webkit.messageHandlers.agent.postMessage(s);
      } else if (window.__ipc) {
        window.__ipc(s);
      }
    } catch (e) {}
  }
  function setOperator(name) {
    OPERATOR = name || OPERATOR;
    document.getElementById('opname').textContent = OPERATOR;
    document.getElementById('avatar').textContent = (OPERATOR.trim()[0] || '?').toUpperCase();
    document.getElementById('emptyTitle').textContent = 'Say hi to ' + OPERATOR + ' 👋';
  }
  function setStatus(t) { statusEl.textContent = t; }
  function setConnected(on) { dot.classList.toggle('on', !!on); }

  function fmtTime(ms) {
    try { return new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }); }
    catch (e) { return ''; }
  }
  function dayKey(ms) { var d = new Date(ms); return d.toDateString(); }
  function nearBottom() { return log.scrollHeight - log.scrollTop - log.clientHeight < 60; }

  function push(line) {
    if (empty) { empty.remove(); empty = null; }
    var from = line.from === 'device' ? 'out' : 'in';
    var dk = dayKey(line.ts_ms);
    if (dk !== lastDay) {
      var sep = document.createElement('div'); sep.className = 'day';
      var d = new Date(line.ts_ms), today = new Date();
      sep.textContent = d.toDateString() === today.toDateString() ? 'Today' : d.toLocaleDateString([], { month: 'short', day: 'numeric' });
      log.appendChild(sep); lastDay = dk; lastFrom = null;
    }
    var grouped = from === lastFrom;
    if (lastRow && grouped) { var pt = lastRow.querySelector('.time'); if (pt) pt.remove(); }
    var row = document.createElement('div');
    row.className = 'row ' + from + (grouped ? ' grouped' : ' first');
    var b = document.createElement('div'); b.className = 'bubble'; b.textContent = line.text;
    var t = document.createElement('div'); t.className = 'time'; t.textContent = fmtTime(line.ts_ms);
    row.appendChild(b); row.appendChild(t);
    log.appendChild(row);
    lastFrom = from; lastRow = row;
    if (from === 'out' || nearBottom()) { log.scrollTop = log.scrollHeight; pill.classList.remove('show'); }
    else { pill.classList.add('show'); }
  }

  window.__agent = { push: push, setOperator: setOperator, setStatus: setStatus, setConnected: setConnected };

  pill.addEventListener('click', function () { log.scrollTop = log.scrollHeight; pill.classList.remove('show'); });
  log.addEventListener('scroll', function () { if (nearBottom()) pill.classList.remove('show'); });

  function autosize() { box.style.height = 'auto'; box.style.height = Math.min(box.scrollHeight, 120) + 'px'; send.disabled = box.value.trim() === ''; }
  box.addEventListener('input', autosize);
  function doSend() {
    var t = box.value.trim(); if (!t) return;
    ipc({ type: 'send', text: t });
    box.value = ''; autosize(); box.focus();
  }
  send.addEventListener('click', doSend);
  box.addEventListener('keydown', function (e) {
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); doSend(); }
  });
  document.getElementById('end').addEventListener('click', function () {
    if (window.confirm('End the remote support session now?')) ipc({ type: 'disconnect' });
  });

  setOperator(OPERATOR);
  window.addEventListener('load', function () { box.focus(); ipc({ type: 'ready' }); });
  ipc({ type: 'ready' });
})();
</script>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_is_escaped_into_the_page() {
        let html = chat_html("A\"</script>B");
        assert!(html.contains(r#"var OPERATOR = "A\"</script>B";"#) || html.contains("A\\\""));
        assert!(!html.contains("__OPERATOR__"));
        assert!(html.contains("window.__agent"));
    }
}
