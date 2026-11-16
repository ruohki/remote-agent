# Architecture

Two repositories:

| Repo | What | Stack |
|------|------|-------|
| `remote-agent` | The agent installed on Windows / macOS machines, plus the shared `protocol` crate | Rust |
| `remote-console` | Management server (device registry, enrollment, signaling hub, TURN credentials) with the embedded web UI | Rust (axum, sqlx/SQLite) + React/Vite/Tailwind SPA |

```
┌───────────────┐   HTTPS + WSS (/ws/ui)    ┌───────────────────────┐   WSS (/ws/agent)   ┌──────────────┐
│  Browser UI   │◄─────────────────────────►│    remote-console     │◄───────────────────►│ remote-agent │
│ (React SPA)   │  auth cookie, live state, │  axum + sqlx + hub    │ hello/heartbeat/    │ (Rust)       │
│               │  SDP/ICE relay            │  serves SPA + install │ config, SDP/ICE     │              │
└──────┬────────┘                           └───────────┬───────────┘                     └──────┬───────┘
       │                                                │ docker-compose sibling                 │
       │            WebRTC (DTLS/SRTP, P2P)             ▼                                         │
       │  video track (H265 | H264) + data channels  ┌────────┐  relay fallback (TURN)             │
       └────────────────────────────────────────────►│ coturn │◄────────────────────────────────────┘
                                                     └────────┘
```

## Data flow of a session

1. Operator opens a device in the UI. The browser creates an `RTCPeerConnection`, adds a
   `recvonly` video transceiver with `setCodecPreferences([H265…, H264…])`, creates the
   `input` and `control` data channels, creates an offer and sends
   `UiToConsole::SessionOffer`.
2. Console creates a session row, mints short-lived TURN credentials (HMAC, coturn
   `static-auth-secret`), replies `ConsoleToUi::SessionCreated { ice_servers }` and forwards
   `ConsoleToAgent::SessionRequest { offer, ice_servers, operator }` to the agent.
3. Agent: in **help-me** mode it shows a native approval dialog (auto-deny after
   `approval_timeout_s`) and reports `ApprovalResult`. In **unattended** mode it continues
   immediately. It builds a peer connection with the codecs it can *encode*, applies the
   offer, creates the answer, reads the negotiated codec, sends `SessionAnswer { answer, codec }`.
4. Console relays the answer and trickle ICE candidates in both directions.
5. Agent starts the capture → encode thread for the primary display and writes Annex-B
   access units into the `TrackLocalStaticSample`. The browser renders the track in a
   `<video>` element; mouse/keyboard go over `input`, everything else over `control`.
6. Either side ends the session (`SessionEnd`, tab close, "Disconnect" on the device banner);
   the console records `end_reason`, pushes `SessionUpdate` to all UIs.

## Codec policy

* Agent advertises `capabilities.codecs` = `[H265 (if hardware), H264]`.
* Browser puts H265 first in codec preferences when `RTCRtpReceiver.getCapabilities('video')`
  lists `video/H265` (Safari, Chrome ≥ 136 with hardware decode); otherwise H264.
* Agent encodes whatever was negotiated; hardware first, `openh264` as the last resort.
* All encoders emit in-band VPS/SPS/PPS before every keyframe, no B-frames, ≤1 frame latency.

## Agent process model

An installed agent is three roles. They are one binary; the subcommand picks the role.

```
                 ┌──────────────────────────────── one machine ─────────────────────────────────┐
console ◄─WSS──► │ service (`service run`)              LocalSystem service / root LaunchDaemon │
                 │  identity + secret, console link, policy, updates, Ctrl+Alt+Del               │
                 │        ▲ local link: named pipe \\.\pipe\RemoteAgent.service             │
                 │        │             or /var/run/remote-agent/service.sock                    │
                 │        ▼                                                                      │
                 │ session host (`session-host`)        one per desktop session                  │
                 │  WebRTC, capture, encode, input, audio, clipboard, files                      │
                 │        ▲ Windows only: UI link \\.\pipe\RemoteAgent.ui.<session>          │
                 │        ▼                                                                      │
                 │ UI (`ui --session N`)                the signed-in user                       │
                 │  tray, window, chat, session bar, approval prompt, privacy screen             │
                 └───────────────────────────────────────────────────────────────────────────────┘
```

* **Service** (`daemon.rs`, `hub.rs`): owns the device identity in the machine state directory
  (`%ProgramData%\RemoteAgent`, `/Library/Application Support/RemoteAgent`) and the console
  connection, so the device is **online whether or not anyone is signed in**. It keeps the
  console's policy plus the local restrictions and hands the effective settings to the hosts.
  Session signaling (`session_request`, `ice_candidate`, `session_end`) is relayed over the local
  link to the host whose desktop is on the console (`HostRouter`); everything the session says
  goes back the same way. A session request with no reachable desktop ends at once
  (`connection_failed`). The service applies updates (it can write the install location) and is
  the only process allowed to generate Ctrl+Alt+Del on Windows.
* **Session host** (`session_host.rs`, `local_sessions.rs`): the session code that used to be
  the whole agent. It reports its `HostStatus` (capabilities, desktop state, signed-in user,
  whether its session is on the console) and the service forwards the active host's status in
  `hello`/heartbeats (`desktop`, `capabilities`, protocol 2).
  * **Windows**: the service starts it with its own **SYSTEM token moved into the console
    session** (`spawn_system_in_session`). Its capture, pipeline, cursor and input threads follow
    the **input desktop** (`platform/windows_desktop.rs`: `OpenInputDesktop` + `SetThreadDesktop`,
    re-attached on every DXGI `ACCESS_LOST`), so one host covers the sign-in screen, the lock
    screen, UAC prompts and the user's desktop, with nobody signed in. It is replaced when the
    console moves to another session (sign-out, fast user switching); locking changes nothing.
  * **macOS**: LaunchAgents — `com.remoteagent.agent` (`LimitLoadToSessionType = Aqua`, the app
    loop in-process) in each signed-in user's GUI session and `com.remoteagent.loginwindow`
    (`LoginWindow`, headless) at the login window. Only the host on the console gets sessions; a
    host whose user is switched away gives its session up. Screen Recording and Accessibility are
    TCC grants of the executable (cannot be pre-approved by MDM for Screen Recording).
* **UI** (Windows only, `app/remote.rs`): a SYSTEM process must not show the user a WebView, so
  the app (tray, window, chat, session bar, approval prompt, privacy screen, annotations) runs
  with the signed-in user's token and the host forwards `crate::app` calls to it (`UiCommand` /
  `UiEvent`). Without a UI process (nobody signed in) help-me requests are denied and the
  privacy screen is unavailable. The UI removes any privacy screen by itself when the host link
  drops or its heartbeats stop for 10 s.
* **Links**: length-prefixed JSON (`link/frame.rs`). Only this executable may connect: every
  peer's image path is checked (`GetNamedPipeClientProcessId` / `LOCAL_PEERPID` →
  `proc_pidpath`); the service pipe additionally admits only SYSTEM and Administrators.
* **Supervision**: the Windows service (`service/windows_service_impl.rs`) restarts host and UI
  with exponential backoff (reset after a minute of uptime) and ties them to a kill-on-close job
  object; SCM recovery actions restart the service after crashes and after an update (which exits
  with a failure code on purpose). launchd `KeepAlive` does the same on macOS.
* **Secrets**: the machine identity's secret is DPAPI **machine scope** in an `agent.toml` whose
  DACL admits only SYSTEM and Administrators (Windows), or in the 0600 root-owned `agent.toml`
  (macOS daemon). `service install` migrates identities enrolled under the old per-user layout.
* **Standalone** (`run`, double-clicked app without the service): the hub and the sessions in one
  user process, as before — used for quick support and development. With the service installed,
  a double-click opens the UI on Windows and never starts a second agent with the same identity.
* **Stopping**: `SIGTERM` / `SIGINT` / `SIGHUP`, the Windows service stop, the tray *Quit* item
  and a completed update all go through `shutdown::request`. The service tells each host to end
  its session (the operator's viewer gets `session_state: ended`) before it closes the console
  socket; only hosts that do not exit in time are terminated. Release builds use
  `panic = "abort"`, so none of it relies on destructors.
* **Privacy screen** (`privacy.rs` + `app/privacy.rs`): on `set_privacy_screen` the device's
  displays show a branded "Screen hidden" page (one opaque, focused, capture-excluded window per
  display; a heavily downsampled snapshot of the desktop as backdrop) while the operator keeps
  seeing the desktop. Gates, all agent-side: `AgentConfig.allow_privacy_screen` (console policy,
  default off, tightenable locally), `SessionRequest.privacy_screen_allowed` (the console's
  `manage` check), device support (`hello.capabilities.privacy_screen`) and not control-paused.
  The guarantee that it comes back lives in
  `privacy::PrivacyGuard`: a dedicated OS thread releases on missed keepalives (5 s), the hard cap
  (30 min) or a display change; releases also fire on session end, control-channel close, the
  pause switch, policy tightening and shutdown; if the UI thread does not confirm a release within
  10 s the process aborts so the supervisor restarts it with the desktop visible. The person at
  the device lifts it with *Show screen* / `Esc`; the operator may engage it again afterwards, and
  pausing control at the device keeps it off for as long as the pause lasts.
* **`remote-agent privacy-probe`** (hidden): measures on the running machine whether the
  agent's own windows — and the window configurations a privacy screen would use — stay out of
  the capture the operator sees, by painting sentinel windows and reading them back through the
  real capture pipeline. Prints a table (`--json` for JSON) and writes the report to the log dir.

## Modes

| Mode | Behaviour |
|------|-----------|
| `unattended` | Operators connect any time — including at the sign-in screen and the lock screen, so an operator can sign in to the machine (installed service; Windows covers UAC prompts and Ctrl+Alt+Del too). |
| `help_me` | The person at the device must click **Allow** on a native dialog for every session. With nobody signed in there is nobody to ask, so the request is denied. |

The mode is a per-device setting in the console (`AgentConfig.mode`) and can be changed live;
the agent applies it on the next `SessionRequest`. The console sees what the screen shows in
`DeviceSummary.desktop` (`user`, `locked`, `sign_in`).

## One-line install

The console renders scripts with the enrollment token and its own URL baked in:

* macOS: `curl -fsSL https://console.example.com/install.sh?token=<TOKEN> | sudo sh`
* Windows (admin PowerShell): `irm https://console.example.com/install.ps1?token=<TOKEN> | iex`

Scripts download the matching release binary (`AGENT_DOWNLOAD_BASE`, default GitHub Releases of
`ruohki/remote-agent`), verify `SHA256SUMS`, run `remote-agent enroll --server … --token …`,
then `remote-agent service install`.

## Protocol

Single source of truth: `crates/protocol` (this repo). `cargo test -p protocol` writes
TypeScript bindings to `bindings/`; the console's `web/` copies them via `npm run sync-protocol`.
The console server depends on the crate through a path dependency to a sibling checkout
(`../../remote-agent/crates/protocol`) — CI checks both repos out side by side.

## Security notes

* Device secret: 32 random bytes, shown once at enrollment, stored hashed (argon2id) on the console.
* Web sessions: opaque random ids in an `HttpOnly; Secure; SameSite=Lax` cookie, argon2id passwords.
* Roles: `admin` (users, tokens, settings) and `operator` (connect to devices).
* Media is end-to-end DTLS-SRTP between browser and agent; the console only sees SDP/ICE.
* TURN credentials expire after 1 h and are bound to the session id.
