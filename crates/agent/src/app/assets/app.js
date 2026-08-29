// Agent application UI. Talks to Rust over wry IPC.
//   JS → Rust: window.ipc.postMessage(JSON.string)  -- {type:"ready"|"send"|"disconnect"|"open_screen"|"install"}
//   Rust → JS: window.__app.<fn>(...)               -- see bottom.
'use strict';
(function () {
  var OPERATOR = 'Support technician';
  var controlPaused = false;
  var annotationsActive = false;
  function renderAnnotations() {
    var n = $('annotNote'); if (n) n.hidden = !annotationsActive;
  }
  function renderPaused() {
    var note = $('pausedNote'); if (note) note.hidden = !controlPaused;
    var meta = $('sessionMeta'); if (meta) meta.textContent = controlPaused ? 'Screen sharing in progress — remote control paused' : 'Screen sharing and remote control in progress';
    var b = $('pauseBtn');
    if (b) { b.textContent = controlPaused ? 'Resume control' : 'Pause control'; b.classList.toggle('solid', controlPaused); }
    var cs = $('chatStatus'); if (cs && cs.textContent.indexOf('Session active') === 0) cs.textContent = controlPaused ? 'Session active · remote control paused' : 'Session active';
  }
  document.addEventListener('click', function (e) {
    var b = e.target.closest && e.target.closest('#pauseBtn');
    if (b) ipc({ type: 'pause_control', paused: !controlPaused });
  });
  var connected = false; // a session is attached
  var hadSession = false; // a session ran in this window at some point (keeps the transcript)
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
  var connectRequired = false; // not enrolled: only Connect + About are reachable
  var installable = false;
  function applyRail() {
    $('tab-connect').hidden = !connectRequired;
    ['home', 'chat', 'settings'].forEach(function (s) { $('tab-' + s).hidden = connectRequired; });
    $('tab-install').hidden = connectRequired || !installable;
  }
  function show(screen) {
    if (connectRequired && screen !== 'connect' && screen !== 'about') screen = 'connect';
    current = screen;
    ['connect', 'home', 'chat', 'install', 'settings', 'about'].forEach(function (s) {
      var el = $('screen-' + s); if (el) el.hidden = s !== screen;
      var tab = $('tab-' + s); if (tab) tab.classList.toggle('active', s === screen);
      var main = document.querySelector('.main'); if (main) main.classList.toggle('chat-mode', screen === 'chat');
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
    transcript.innerHTML = '<div class="empty" id="chatEmpty">No messages.</div>';
    lastFrom = null; lastDay = null; unread = 0; renderBadge();
  }

  function autosize() {
    composer.style.height = 'auto';
    composer.style.height = Math.max(44, Math.min(composer.scrollHeight, 150)) + 'px';
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
    if (window.confirm('End the support session now?')) ipc({ type: 'disconnect' });
  }
  $('endBtn').addEventListener('click', confirmEnd);
  $('chatEndBtn').addEventListener('click', confirmEnd);
  $('openChatBtn').addEventListener('click', function () { show('chat'); });
  // ---- permissions (macOS) & install location ----
  function bindPermissionButtons() {
    document.querySelectorAll('[data-request]').forEach(function (b) {
      b.addEventListener('click', function () { ipc({ type: 'request_permission', which: b.getAttribute('data-request') }); });
    });
    document.querySelectorAll('[data-settings]').forEach(function (b) {
      b.addEventListener('click', function () { ipc({ type: 'open_settings', which: b.getAttribute('data-settings') }); });
    });
    $('permWarnBtn').addEventListener('click', function () { show('home'); ipc({ type: 'request_permission', which: 'screen' }); });
    $('moveBtn').addEventListener('click', function () {
      $('moveBtn').disabled = true;
      setStatus2($('moveStatus'), 'Moving…', '');
      ipc({ type: 'move_to_applications' });
    });
  }
  bindPermissionButtons();
  function renderPermissions(p) {
    var card = $('permsCard');
    if (!p || !p.supported) { card.hidden = true; $('permWarn').hidden = true; return; }
    card.hidden = false;
    [['screen', p.screen], ['accessibility', p.accessibility]].forEach(function (pair) {
      var row = document.querySelector('.perm-row[data-perm="' + pair[0] + '"]');
      if (!row) return;
      row.classList.toggle('ok', !!pair[1]);
      var badge = row.querySelector('[data-badge]');
      badge.textContent = pair[1] ? 'Granted' : 'Not granted';
      badge.classList.toggle('ok', !!pair[1]);
    });
    $('permWarn').hidden = !!p.screen;
  }

  $('installBtn').addEventListener('click', function () {
    $('installBtn').disabled = true;
    setStatus2($('installStatus'), 'Administrator authorization required…', '');
    ipc({ type: 'install' });
  });

  // ---- connect (first-run enrollment) ----
  var connectBusy = false;
  var urlCheckTimer = null;
  function fieldError(id, message) {
    var err = $(id + 'Err'), field = $(id).closest('.field');
    err.textContent = message || ''; err.hidden = !message;
    if (field) field.classList.toggle('invalid', !!message);
  }
  function setConnectBusy(on) {
    connectBusy = !!on;
    $('connectSpin').hidden = !on;
    $('connectBtnText').textContent = on ? 'Connecting…' : 'Connect';
    $('connectBtn').disabled = !!on;
    ['connectUrl', 'connectToken', 'connectName'].forEach(function (id) { $(id).disabled = !!on; });
  }
  function requestUrlCheck() {
    var url = $('connectUrl').value.trim();
    if (!url) { fieldError('connectUrl', ''); return; }
    ipc({ type: 'check_url', server_url: url });
  }
  $('connectUrl').addEventListener('input', function () {
    fieldError('connectUrl', '');
    clearTimeout(urlCheckTimer);
    urlCheckTimer = setTimeout(requestUrlCheck, 400);
  });
  $('connectUrl').addEventListener('blur', function () { clearTimeout(urlCheckTimer); requestUrlCheck(); });
  $('connectToken').addEventListener('input', function () { fieldError('connectToken', ''); });
  $('connectForm').addEventListener('submit', function (e) {
    e.preventDefault();
    if (connectBusy) return;
    var url = $('connectUrl').value.trim(), token = $('connectToken').value.trim(), name = $('connectName').value.trim();
    var ok = true;
    if (!url) { fieldError('connectUrl', 'Enter the console URL'); ok = false; }
    if (!token) { fieldError('connectToken', 'Enter the enrollment token'); ok = false; }
    if (!ok) return;
    $('connectErr').hidden = true;
    setConnectBusy(true);
    ipc({ type: 'connect', server_url: url, token: token, name: name || null });
  });
  function renderConnect(s) {
    if (s.state === 'show') {
      connectRequired = true;
      $('connectUrl').value = s.server_url || '';
      $('connectUrl').readOnly = !!s.locked;
      $('connectToken').value = '';
      $('connectName').value = s.name || '';
      fieldError('connectUrl', ''); fieldError('connectToken', '');
      var n = $('connectNotice'); n.textContent = s.error || ''; n.hidden = !s.error;
      $('connectErr').hidden = true;
      setConnectBusy(false);
      applyRail();
      $('railStatus').textContent = 'Not enrolled';
      $('railDot').classList.remove('on'); $('railDot').classList.add('warn');
      show('connect');
      (s.server_url ? $('connectToken') : $('connectUrl')).focus();
    } else if (s.state === 'busy') {
      setConnectBusy(true);
    } else if (s.state === 'failed') {
      setConnectBusy(false);
      var el = $('connectErr'); el.textContent = s.message || 'Enrollment failed'; el.hidden = false;
      $('connectToken').focus();
    } else if (s.state === 'done') {
      connectRequired = false;
      setConnectBusy(false);
      $('connectToken').value = '';
      applyRail();
      $('railStatus').textContent = 'Connecting…';
      show('home');
    }
  }
  $('reenrollBtn').addEventListener('click', function () {
    if (window.confirm('Disconnect from the console and enroll this device again?')) ipc({ type: 'reenroll' });
  });

  function setConnected(on) {
    connected = !!on;
    $('sessionActive').hidden = !on;
    $('sessionNone').hidden = !!on;
    $('chatEndBtn').hidden = !on;
    $('chatDot').classList.toggle('warn', !on); $('chatDot').classList.toggle('on', !!on);
    composer.disabled = !on;
    composer.placeholder = on ? 'Message…' : 'Chat becomes available when a support technician is connected';
    var chatTab = $('tab-chat'); if (chatTab) chatTab.classList.toggle('inactive', !on);
    autosize();
    var chat = document.querySelector('.chat'); if (chat) chat.classList.toggle('no-session', !on);
    var ns = $('chatNoSession'); if (ns) ns.hidden = !!on || hadSession;
    $('chatStatus').textContent = on ? (controlPaused ? 'Session active · remote control paused' : 'Session active') : (hadSession ? 'Session ended' : 'No active session');
    if (on) hadSession = true;
  }
  function setStatus2(el, text, cls) { el.textContent = text; el.className = 'install-status' + (cls ? ' ' + cls : ''); }


  // ---- settings (privacy & control) ----
  // policy: { console:{mode,allow_input,...}, overrides:{...}, effective:{...} }
  var policy = null;
  var SWITCH_KEYS = ['require_approval', 'allow_input', 'allow_audio', 'allow_clipboard', 'allow_file_transfer', 'allow_annotations'];

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
    var idle = $('sessionNoneText');
    if (idle) idle.textContent = eff.mode === 'help_me'
      ? 'A support technician can connect to this computer after your approval.'
      : 'A support technician can connect to this computer.';
    SWITCH_KEYS.forEach(function (key) {
      var sw = document.querySelector('.switch[data-key="' + key + '"]');
      var sub = document.querySelector('.setting[data-key="' + key + '"] [data-policy]');
      if (!sw) return;
      var locked, checked, note;
      if (key === 'require_approval') {
        var consoleRequires = c.mode === 'help_me';
        checked = consoleRequires || eff.mode === 'help_me';
        locked = consoleRequires; // administrator already requires approval
        note = consoleRequires ? 'Required by administrator policy' : 'Administrator policy: sessions may start without approval';
      } else {
        var consoleBlocks = c[key] === false;
        checked = !consoleBlocks && eff[key] !== false; // allowed only if console allows AND not locally blocked
        locked = consoleBlocks; // cannot loosen what the administrator blocked
        note = consoleBlocks ? 'Disabled by administrator policy' : 'Permitted by administrator policy';
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
        allow_file_transfer: switchState('allow_file_transfer'),
        allow_annotations: switchState('allow_annotations')
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
      OPERATOR = operator || 'Support technician';
      $('opName').textContent = OPERATOR;
      $('opAvatar').textContent = initials(OPERATOR);
      $('chatAvatar').textContent = initials(OPERATOR);
      $('chatWith').textContent = OPERATOR;
      clearTranscript();
      setConnected(true);
    },
    endSession: function () { controlPaused = false; renderPaused(); setConnected(false); },
    setControlPaused: function (on) { controlPaused = !!on; renderPaused(); },
    setAnnotations: function (on) { annotationsActive = !!on; renderAnnotations(); },
    push: push,
    // Console / device status
    setConsole: function (url, connectedToConsole) {
      var host = (url || '').replace(/^[a-z]+:\/\//i, '').replace(/\/.*$/, '');
      $('consoleUrl').textContent = host || '—';
      $('consoleUrl').title = url || '';
      $('aboutConsole').textContent = url || '—';
      $('reenrollSub').textContent = host ? 'Enrolled with ' + host : '—';
      $('consoleState').textContent = connectedToConsole ? 'Connected' : 'Not connected';
      $('consoleDot').classList.toggle('on', !!connectedToConsole);
      $('railStatus').textContent = connectedToConsole ? 'Online' : 'Offline';
      $('railDot').classList.toggle('on', !!connectedToConsole);
      $('railDot').classList.toggle('warn', !connectedToConsole);
    },
    setDevice: function (name, id) { $('deviceName').textContent = name || '—'; $('deviceId').textContent = id || '—'; },
    // Branding
    setBranding: function (b) {
      if (b.accent) document.documentElement.style.setProperty('--accent', b.accent);
      var name = b.product_name || 'Remote Support';
      document.title = name;
      $('brandName').textContent = name;
      $('heroName').textContent = name;
      $('installProduct').textContent = name;
      $('aboutProduct').textContent = name;
      $('aboutOrg').textContent = b.organization || '—';
      $('supportText').textContent = b.support_text || '';
      var logoUrl = b.logo ? 'url("data:image/png;base64,' + b.logo + '")' : '';
      ['logo', 'heroLogo', 'connectLogo'].forEach(function (id) {
        $(id).style.backgroundImage = logoUrl;
        $(id).classList.toggle('has-logo', !!b.logo);
      });
    },
    setAbout: function (version, keyfp) { $('aboutVersion').textContent = version || '—'; $('aboutKey').textContent = keyfp || '—'; },
    setInstallable: function (yes) { installable = !!yes; applyRail(); },
    // First-run enrollment
    setConnect: renderConnect,
    urlCheck: function (r) { fieldError('connectUrl', r && !r.ok ? r.message : ''); },
    installResult: function (ok, message) {
      $('installBtn').disabled = ok;
      setStatus2($('installStatus'), message, ok ? 'ok' : 'err');
    },
    // Privacy & control policy
    setPolicy: function (p) { policy = p; renderPolicy(); },
    // macOS permissions + install location onboarding
    setPermissions: renderPermissions,
    setLocation: function (l) {
      var card = $('moveCard');
      card.hidden = !(l && l.movable);
      if (l && l.path) $('movePath').textContent = (l.translocated ? 'Running from a temporary Gatekeeper location. ' : 'Running from ' + l.path + '. ') + 'Move it to the Applications folder so permissions and settings are kept.';
    },
    moveResult: function (ok, message) { $('moveBtn').disabled = ok; setStatus2($('moveStatus'), message, ok ? 'ok' : 'err'); },
    // Navigation from Rust (tray "open chat")
    show: show,
  };

  autosize();
  setConnected(false);
  window.addEventListener('load', function () { ipc({ type: 'ready' }); });
  ipc({ type: 'ready' });
})();
