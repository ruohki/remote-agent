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
* One binary, installs as a Windows service / launchd daemon, enrolls with a single command
* Reachable with nobody signed in: sign-in screen, lock screen, UAC prompts and Ctrl+Alt+Del
  (Windows), login window (macOS) — see ARCHITECTURE.md, *Agent process model*

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

macOS builds need Xcode command line tools; Windows builds need the MSVC toolchain. Windows
binaries link the C runtime statically (`.cargo/config.toml`, `build/msvc-static-crt.cmake`):
check with `llvm-readobj --coff-imports remote-agent.exe` that no `VCRUNTIME140.dll` is imported.
Cross-build Windows from macOS with `cargo xwin build --features winservice --target
x86_64-pc-windows-msvc -p remote-agent` (LLVM on `PATH`, and
`REMOTE_AGENT_CHAIN_TOOLCHAIN=<cargo-xwin cache>/cmake/clang-cl/x86_64-pc-windows-msvc-toolchain.cmake`).

## CLI

```
remote-agent enroll --server https://console.example.com --token TOKEN [--name "Front desk"] [--if-needed]
remote-agent service install|uninstall|start|stop   # install is idempotent (re-run to upgrade)
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
