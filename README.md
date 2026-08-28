# remote-agent

Remote desktop agent for Windows and macOS. Pairs with
[`remote-console`](../remote-console) (the management server + web viewer).

* Screen capture with the native zero-copy APIs (ScreenCaptureKit, DXGI desktop duplication)
* Hardware **H.265** encoding with automatic **H.264** fallback (VideoToolbox, Media Foundation, OpenH264)
* WebRTC (DTLS-SRTP) straight to the operator's browser; TURN relay only when needed
* Two modes: **unattended** and **help-me** (the local user must approve each session)
* Multi-display streaming (one track per display), system audio (Opus)
* Resumable, hash-verified file transfer in both directions, remote file browser, image/file clipboard
* Built-in chat with the person at the device; every session reports an event timeline to the console
* One binary, installs as a launchd agent / Windows service, enrolls with a single command

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design and the wire protocol.

## Install (end users)

Run the one-liner shown in the console under *Devices → Add device*:

```sh
# macOS
curl -fsSL https://console.example.com/install.sh?token=TOKEN | sudo sh
```

```powershell
# Windows (administrator PowerShell)
irm https://console.example.com/install.ps1?token=TOKEN | iex
```

## Build

```sh
cargo build --release -p remote-agent
# TypeScript bindings for the protocol (written to bindings/)
cargo test -p protocol
```

macOS builds need Xcode command line tools; Windows builds need the MSVC toolchain.
Cross-check Windows code from macOS with `cargo check --target x86_64-pc-windows-msvc`.

## CLI

```
remote-agent enroll --server https://console.example.com --token TOKEN [--name "Front desk"]
remote-agent service install|uninstall|start|stop
remote-agent run        # foreground (what the service runs)
remote-agent status
remote-agent doctor     # permissions, displays, encoders
remote-agent reset      # forget enrollment
```

## Layout

```
crates/protocol   shared wire types (serde + ts-rs)
crates/agent      the agent binary
  src/capture     ScreenCaptureKit / DXGI
  src/encode      VideoToolbox / Media Foundation / OpenH264
  src/session     WebRTC peer connection, video pipeline, data channels
  src/hub         WebSocket client to the console
  src/service     launchd / Windows service
```

## License

AGPL-3.0-only.
