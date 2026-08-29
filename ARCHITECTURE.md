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

* **macOS**: a per-user LaunchAgent runs `remote-agent run` in the GUI session (screen
  recording + accessibility permissions are per-user/TCC). Capture: ScreenCaptureKit.
  Encode: VideoToolbox. Input: CGEvent via `enigo`.
* **Windows**: a LocalSystem service (`remote-agent service run`) supervises one
  `remote-agent run` child in the active console session (`CreateProcessAsUser`, `winsta0\default`),
  so UAC prompts and the logon screen are reachable. Capture: DXGI desktop duplication.
  Encode: Media Foundation hardware MFT (NV12 via `ID3D11VideoProcessor`). Input: `SendInput` via `enigo`.
* One binary; `run` is the actual agent, everything else is CLI plumbing.
* State lives in `/Library/Application Support/RemoteAgent/agent.toml` or
  `%ProgramData%\RemoteAgent\agent.toml` (owner-only permissions; contains the device secret).
* **Stopping**: `SIGTERM` / `SIGINT` / `SIGHUP` (`launchctl kill`, `bootout`, Ctrl-C), the Windows
  console control events, the tray *Quit* item and a completed update all go through
  `shutdown::request`. The hub ends the active session (keys released, overlays and session bar
  removed, `session_state: ended` sent), flushes, closes the console socket with a reason and
  returns; the process exits with code 0 and the service manager restarts it where configured.
  Quit has a 6 s backstop. Windows `TerminateProcess` from the service supervisor bypasses all of
  this (known gap). Release builds use `panic = "abort"`, so none of it relies on destructors.
* **Privacy screen** (`privacy.rs` + `app/privacy.rs`): on `set_privacy_screen` the device's
  displays show a branded "Screen hidden" page (one opaque, focused, capture-excluded window per
  display; a heavily downsampled snapshot of the desktop as backdrop) while the operator keeps
  seeing the desktop. Gates, all agent-side: `AgentConfig.allow_privacy_screen` (console policy,
  default off, tightenable locally), `SessionRequest.privacy_screen_allowed` (the console's
  `manage` check), device support (`hello.capabilities.privacy_screen`), not control-paused, not
  lifted by the device user earlier in the session. The guarantee that it comes back lives in
  `privacy::PrivacyGuard`: a dedicated OS thread releases on missed keepalives (5 s), the hard cap
  (30 min) or a display change; releases also fire on session end, control-channel close, the
  pause switch, policy tightening and shutdown; if the UI thread does not confirm a release within
  10 s the process aborts so the supervisor restarts it with the desktop visible. The person at
  the device lifts it with *Show screen* / `Esc`; after that it stays off for the session.
* **`remote-agent privacy-probe`** (hidden): measures on the running machine whether the
  agent's own windows — and the window configurations a privacy screen would use — stay out of
  the capture the operator sees, by painting sentinel windows and reading them back through the
  real capture pipeline. Prints a table (`--json` for JSON) and writes the report to the log dir.

## Modes

| Mode | Behaviour |
|------|-----------|
| `unattended` | Operators connect any time. |
| `help_me` | The person at the device must click **Allow** on a native dialog for every session. |

The mode is a per-device setting in the console (`AgentConfig.mode`) and can be changed live;
the agent applies it on the next `SessionRequest`.

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
