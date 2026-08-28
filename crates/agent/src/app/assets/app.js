// Agent application UI. Talks to Rust over wry IPC.
//   JS → Rust: window.ipc.postMessage(JSON.string)  -- {type:"ready"|"send"|"disconnect"|"open_screen"|"install"}
//   Rust → JS: window.__app.<fn>(...)               -- see bottom.
'use strict';
(function () {
  var OPERATOR = 'Operator';
  var connected = false; // a session is attached
  var unread = 0;
  var lastFrom = null, lastDay = null;

  function ipc(obj) {
    var s = JSON.stringify(obj);
    try {
      if (window.ipc && window.ipc.postMessage) window.ipc.postMessage(s);
    } catch (e) {}
  }
  function $(id) { return document.getElementById(id); }
  function initials(name) { return (String(name).trim()[0] || '?').toUpperCase(); }

  // ---- navigation ----
  var current = 'home';
  function show(screen) {
    current = screen;
    ['home', 'chat', 'install', 'settings', 'about'].forEach(function (s) {
      var el = $('screen-' + s); if (el) el.hidden = s !== screen;
      var tab = $('tab-' + s); if (tab) tab.classList.toggle('active', s === screen);
    });
    if (screen === 'chat') { unread = 0; renderBadge(); $('composer').focus(); scrollDown(); }
    ipc({ type: 'open_screen', screen: screen });
  }
  document.querySelectorAll('.tab').forEach(function (t) {
    t.addEventListener('click', function () { show(t.getAttribute('data-screen')); });
  });

  // ---- chat ----
  var transcript = $('transcript'), composer = $('composer'), sendBtn = $('send');
  function scrollDown() { transcript.scrollTop = transcript.scrollHeight; }
  function dayLabel(ts) {
    var d = new Date(ts), n = new Date();
    var same = d.toDateString() === n.toDateString();
    if (same) return 'Today';
    var y = new Date(n.getTime() - 864e5);
    if (d.toDateString() === y.toDateString()) return 'Yesterday';
    return d.toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric' });
  }
  function hhmm(ts) { return new Date(ts).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' }); }

  function push(msg) {
    var empty = $('chatEmpty'); if (empty) empty.remove();
    var day = dayLabel(msg.ts_ms);
    if (day !== lastDay) {
      var sep = document.createElement('div'); sep.className = 'daysep'; sep.textContent = day;
      transcript.appendChild(sep); lastDay = day; lastFrom = null;
    }
    var me = msg.from === 'device';
    var row = document.createElement('div');
    row.className = 'row ' + (me ? 'me' : 'op') + (lastFrom === msg.from ? ' grouped' : '');
    var bubble = document.createElement('div'); bubble.className = 'bubble'; bubble.textContent = msg.text;
    var time = document.createElement('div'); time.className = 'time'; time.textContent = hhmm(msg.ts_ms);
    row.appendChild(bubble); row.appendChild(time);
    transcript.appendChild(row);
    lastFrom = msg.from;
    var atBottom = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 80;
    if (atBottom || me) scrollDown();
    if (!me && current !== 'chat') { unread++; renderBadge(); }
  }
  function renderBadge() {
    var b = $('chatBadge');
    if (unread > 0) { b.hidden = false; b.textContent = unread > 99 ? '99+' : String(unread); }
    else b.hidden = true;
  }
  function clearTranscript() {
    transcript.innerHTML = '<div class="empty" id="chatEmpty">Say hi to ' + OPERATOR + '.</div>';
    lastFrom = null; lastDay = null; unread = 0; renderBadge();
  }

  function autosize() {
    composer.style.height = 'auto';
    composer.style.height = Math.min(composer.scrollHeight, 120) + 'px';
    sendBtn.disabled = !connected || composer.value.trim() === '';
  }
  composer.addEventListener('input', autosize);
  composer.addEventListener('keydown', function (e) {
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); doSend(); }
  });
  function doSend() {
    if (!connected) return;
    var t = composer.value.trim(); if (!t) return;
    ipc({ type: 'send', text: t });
    composer.value = ''; autosize(); unread = 0; renderBadge();
  }
  sendBtn.addEventListener('click', doSend);

  function confirmEnd() {
    if (window.confirm('End the remote support session now?')) ipc({ type: 'disconnect' });
  }
  $('endBtn').addEventListener('click', confirmEnd);
  $('chatEndBtn').addEventListener('click', confirmEnd);
  $('openChatBtn').addEventListener('click', function () { show('chat'); });
  $('installBtn').addEventListener('click', function () {
    $('installBtn').disabled = true;
    setStatus2($('installStatus'), 'Requesting administrator permission…', '');
    ipc({ type: 'install' });
  });

  function setConnected(on) {
    connected = !!on;
    $('sessionActive').hidden = !on;
    $('sessionNone').hidden = !!on;
    $('chatEndBtn').hidden = !on;
    ['railDot', 'chatDot'].forEach(function (id) { $(id).classList.toggle('warn', !on); $(id).classList.toggle('on', !!on); });
    composer.disabled = !on;
    autosize();
    $('chatStatus').textContent = on ? 'Connected' : 'Session ended';
  }
  function setStatus2(el, text, cls) { el.textContent = text; el.className = 'install-status' + (cls ? ' ' + cls : ''); }


  // ---- settings (privacy & control) ----
  // policy: { console:{mode,allow_input,...}, overrides:{...}, effective:{...} }
  var policy = null;
  var SWITCH_KEYS = ['require_approval', 'allow_input', 'allow_audio', 'allow_clipboard', 'allow_file_transfer'];

  function consoleAllows(key) {
    if (!policy) return true;
    var c = policy.console || {};
    if (key === 'require_approval') return true; // console can only *require* it; never forces "no approval"
    return c[key] !== false;
  }
  // Current local checked-state of a switch (true = allowed / not-restricted; for require_approval true = required).
  function switchState(key) {
    var el = document.querySelector('.switch[data-key="' + key + '"]');
    return el ? el.getAttribute('aria-checked') === 'true' : (key !== 'require_approval');
  }
  function renderPolicy() {
    if (!policy) return;
    var c = policy.console || {}, eff = policy.effective || {};
    SWITCH_KEYS.forEach(function (key) {
      var sw = document.querySelector('.switch[data-key="' + key + '"]');
      var sub = document.querySelector('.setting[data-key="' + key + '"] [data-policy]');
      if (!sw) return;
      var locked, checked, note;
      if (key === 'require_approval') {
        var consoleRequires = c.mode === 'help_me';
        checked = consoleRequires || eff.mode === 'help_me';
        locked = consoleRequires; // administrator already requires approval
        note = consoleRequires ? 'Required by your administrator' : 'Administrator lets operators connect without asking';
      } else {
        var consoleBlocks = c[key] === false;
        checked = !consoleBlocks && eff[key] !== false; // allowed only if console allows AND not locally blocked
        locked = consoleBlocks; // cannot loosen what the administrator blocked
        note = consoleBlocks ? 'Blocked by your administrator' : 'Allowed by your administrator';
      }
      sw.setAttribute('aria-checked', checked ? 'true' : 'false');
      sw.classList.toggle('locked', !!locked);
      sw.disabled = !!locked;
      if (sub) sub.textContent = note;
    });
  }
  function pushOverrides() {
    ipc({
      type: 'set_overrides',
      overrides: {
        require_approval: switchState('require_approval'),
        allow_input: switchState('allow_input'),
        allow_audio: switchState('allow_audio'),
        allow_clipboard: switchState('allow_clipboard'),
        allow_file_transfer: switchState('allow_file_transfer')
      }
    });
  }
  document.querySelectorAll('.switch').forEach(function (sw) {
    function toggle() {
      if (sw.disabled) return;
      var now = sw.getAttribute('aria-checked') === 'true';
      sw.setAttribute('aria-checked', now ? 'false' : 'true');
      pushOverrides();
    }
    sw.addEventListener('click', toggle);
    sw.addEventListener('keydown', function (e) {
      if (e.key === ' ' || e.key === 'Enter') { e.preventDefault(); toggle(); }
    });
  });

  // ---- Rust → JS API ----
  window.__app = {
    // Session lifecycle
    startSession: function (operator) {
      OPERATOR = operator || 'Operator';
      $('opName').textContent = OPERATOR;
      $('opAvatar').textContent = initials(OPERATOR);
      $('chatAvatar').textContent = initials(OPERATOR);
      $('chatWith').textContent = OPERATOR;
      clearTranscript();
      setConnected(true);
    },
    endSession: function () { setConnected(false); },
    push: push,
    // Console / device status
    setConsole: function (url, connectedToConsole) {
      $('consoleUrl').textContent = url || '—';
      $('aboutConsole').textContent = url || '—';
      $('consoleState').textContent = connectedToConsole ? 'Connected' : 'Connecting…';
      $('consoleDot').classList.toggle('on', !!connectedToConsole);
      $('railStatus').textContent = connectedToConsole ? 'Online' : 'Connecting…';
      $('railDot').classList.toggle('on', !!connectedToConsole && !connected);
    },
    setDevice: function (name, id) { $('deviceName').textContent = name || '—'; $('deviceId').textContent = id || '—'; },
    // Branding
    setBranding: function (b) {
      if (b.accent) document.documentElement.style.setProperty('--accent', b.accent);
      var name = b.product_name || 'Remote Support';
      $('brandName').textContent = name;
      $('heroName').textContent = name;
      $('installProduct').textContent = name;
      $('aboutProduct').textContent = name;
      $('aboutOrg').textContent = b.organization || '—';
      $('supportText').textContent = b.support_text || '';
      if (b.logo) {
        var url = 'url("data:image/png;base64,' + b.logo + '")';
        $('logo').style.backgroundImage = url;
        $('heroLogo').style.backgroundImage = url;
      }
    },
    setAbout: function (version, keyfp) { $('aboutVersion').textContent = version || '—'; $('aboutKey').textContent = keyfp || '—'; },
    setInstallable: function (yes) { $('tab-install').hidden = !yes; },
    installResult: function (ok, message) {
      $('installBtn').disabled = ok;
      setStatus2($('installStatus'), message, ok ? 'ok' : 'err');
    },
    // Privacy & control policy
    setPolicy: function (p) { policy = p; renderPolicy(); },
    // Navigation from Rust (tray "open chat")
    show: show,
  };

  autosize();
  setConnected(false);
  window.addEventListener('load', function () { ipc({ type: 'ready' }); });
  ipc({ type: 'ready' });
})();
