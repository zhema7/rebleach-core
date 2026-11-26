# Rebleach

English | [简体中文](README.md)

A Windows remote-control solution built with **Rust + WebRTC + Tauri**. The controller is a desktop
application with a GUI, while the controlled side is a windowless tray app / Windows service agent.
Both share the same signaling protocol. Rebleach also ships command-line automation (remote commands,
port forwarding, SSH, Ansible) and one-click enrollment through the server.

> This project is intended for remote management of **your own devices or devices you are explicitly
> authorized to access**. Do not use it for unauthorized remote control.

---

## Features

### Remote screen
- **Automatic three-tier capture fallback**: Windows Graphics Capture (WGC) → DXGI Desktop Duplication (DDA) → GDI `BitBlt`,
  so even older systems without a GPU or WGC can still produce frames.
- **GPU downscaling + H.264 encoding**: captured textures are scaled to the target resolution on the GPU before encoding,
  avoiding per-pixel CPU copies. Bitrate and frame rate are adjustable at runtime.
- **Multi-monitor support**: switch the capture target from the controller's dropdown; virtual displays are marked with `💻`.
- **Remote virtual display management**: the agent plugs/unplugs virtual monitors via the usbmmidd driver's `IOCTL_PLUG`.
  New displays are automatically placed to the right of the physical screen, preventing coordinate gaps that make the
  cursor "hit a wall" between monitors.

### Remote input
- **Mouse / keyboard injection** (Win32 `SendInput`): DPI-aware coordinate translation that adds the origin offset of the
  currently captured display in the virtual desktop, so clicks stay accurate with multiple monitors and 200% scaling.
- **Bidirectional clipboard sync** with loop suppression, so the two sides do not overwrite each other repeatedly.
- **Lock-screen unlock**: works together with a UIAccess helper process to inject input on the secure desktop.
- **System key combinations** such as Ctrl+Alt+Del, plus wheel and drag operations.

### Connectivity
- **WebRTC P2P first**: direct connections are used whenever ICE hole punching succeeds; TURN relaying is only used as a fallback.
- **Multiple STUN candidates** improve direct-connect success rates behind symmetric NAT.
- **LAN direct mode**: UDP broadcast discovery plus a built-in mini signaling server on the agent. Devices on the same subnet
  can connect without a public server and authenticate with a LAN passcode.
- **Automatic reconnect** and session keep-alive while the screen is locked (the session is not torn down, so no re-registration is needed after unlock).

### Terminal and CLI automation
- **`exec`**: run one-off remote commands and get the agent's real exit code and plain-text output
  (UTF-8 is set automatically; no ANSI escape sequences).
- **`tunnel`**: map loopback / private-network ports on the agent to `127.0.0.1` on your machine. Destinations are restricted
  to private addresses so the agent cannot be abused as a public jump host.
- **`rebleach-proxy`**: a pure protocol bridge for OpenSSH `ProxyCommand`. SSH into the agent without a public IP or an open port 22.
- **`setup` / `ssh-config` / `inventory`**: generate an operator key and register its public key with the server, render
  `~/.ssh/config.d/rebleach.conf`, and emit an Ansible inventory.
- **Terminal recording**: asciicast v2 by default, with plain-text replay via `term-replay` for searching and auditing.

### Enrollment and one-click deployment
- **Enrollment tokens**: one-time, expiring tokens generated from the admin UI and revocable at any time.
- **One-click install**: run `irm "<server>/bootstrap/install.ps1?token=<token>" | iex` on the target machine to download,
  enroll, and silently install the agent.
- **Centralized key distribution**: controller public keys are stored on the server; agents sync `ssh_authorized_keys`
  every 60 seconds, so authorization changes require no reinstall or restart.

### Server and administration
- Signaling / management service built on `axum`: device registration, online status, grants, audit logs, and package distribution.
- **Built-in admin pages** under `/admin` for devices, grants, packages, audit, and access configuration, protected by Basic Auth.
- **TURN REST ephemeral credentials**: the server signs short-lived TURN accounts with `static-auth-secret`, so long-term
  secrets never reach clients.
- Device identities are signed with **Ed25519** key pairs rather than guessable device numbers.

### Packaging and updates
- Tauri produces two separate installers: `Rebleach Controller` and `Rebleach Agent`.
- The agent installer registers a Windows service and creates a tray auto-start entry; driver components are installed
  with administrator privileges through NSIS hooks.
- **Signed auto-update**: minisign key pairs plus NSIS passive install; updates with invalid signatures are rejected.

---

## Technical highlights

| Highlight | Description |
|---|---|
| Signaling/media separation | H.264 frames bypass Tauri IPC and are streamed to the frontend over a local `127.0.0.1` WebSocket binary channel, avoiding JSON serialization and base64 CPU overhead |
| Capture buffer reuse | Frame pool plus read-back buffer pool reuse allocated `Vec`s per frame, avoiding ~9 MB allocations per frame at 1080p |
| Session 0 → user session | The service runs as SYSTEM and spawns the tray agent in the logged-in session with `CreateProcessAsUser`, combining boot auto-start / windowless operation with access to the real desktop |
| DPI-correct input injection | The capture side sends the "encoded frame → physical coordinates" scale and screen origin to the injection module via `set_mapping` |
| Display layout API | Virtual display layout uses `QueryDisplayConfig` + `SetDisplayConfig` (path/mode arrays) instead of `ChangeDisplaySettingsEx`, which IDD drivers may roll back |
| WOW64 safety | 32-bit processes that need 64-bit PowerShell use `Sysnative` to avoid `SysWOW64` redirection pitfalls |
| Dual-role split | The same crate supports `controller` (GUI) and `agent` (tray/service) builds via feature flags, sharing the `client` protocol library |

---

## Repository layout

```
rebleach-core/
├── client/                 Cross-platform core library (session, WebRTC, capture, injection, protocol) + CLI
│   ├── src/cli.rs          CLI subcommands (exec / tunnel / ssh-config / setup ...)
│   ├── src/sshd.rs         Built-in SSH server and authorized-key sync
│   ├── src/bin/            rebleach-client / rebleach-proxy
│   └── examples/           CLI / LAN end-to-end tests
├── server/                 Signaling / management service (axum)
│   ├── src/main.rs         Server and public enrollment routes (/bootstrap/*, /agent/ssh-keys)
│   └── web/                Admin pages (devices / grants / packages / audit / access)
├── app/
│   ├── src-tauri/          Tauri controller and agent
│   │   ├── src/
│   │   │   ├── controller.rs     Controller session and media channel
│   │   │   ├── agent.rs          Agent session loop
│   │   │   ├── agent_app.rs      Tray agent
│   │   │   ├── service.rs        Windows service
│   │   │   ├── child.rs          CreateProcessAsUser session spawning
│   │   │   ├── virtual_display.rs Virtual display add/remove and layout
│   │   │   └── lan.rs            LAN direct mode
│   │   ├── nsis/                 NSIS installer hooks
│   │   └── resources/            Bundled resources (including virtual display scripts)
│   ├── ui/                 Controller frontend (vanilla HTML/JS)
│   ├── ui-agent/           Agent empty window
│   └── ui-shim/            UIAccess helper process
├── docs/                   Usage and deployment documentation (Chinese)
└── Cargo.toml              Workspace (Rust 2024)
```

---

## Build

Prerequisites: Rust 1.80+, WebView2 Runtime, NSIS (downloaded automatically by Tauri when packaging Windows installers).

```bash
# Core library / server
cargo build --release

# Controller installer
cd app
cargo tauri build

# Agent installer (tray-only agent)
cd app
cargo tauri build --config src-tauri/tauri.agent.conf.json \
  -- --no-default-features --features agent
```

### Run the server

```bash
cargo run -p rebleach-server --release -- \
  --addr 0.0.0.0:8088 \
  --db ./rebleach.db \
  --admin-user admin --admin-pass <admin-password> \
  --packages-dir ./packages \
  --public-url https://signal.example.com \
  --turn-url turn:signal.example.com:3478 \
  --turn-secret <coturn-static-auth-secret>
```

| Flag | Default | Description |
|---|---|---|
| `--addr` | `0.0.0.0:8088` | Listen address |
| `--db` | `./rebleach.db` | SQLite database (schema created on first start) |
| `--admin-user` / `--admin-pass` | `admin` / `admin` | Admin Basic Auth credentials; **change them before going live** |
| `--packages-dir` | `./packages` | Package directory, also used by `/bootstrap/agent` |
| `--public-url` | empty | Public URL used to build one-click install commands; inferred from request headers when empty |
| `--turn-url` / `--turn-secret` | empty | coturn URL and `static-auth-secret`; no TURN credentials are issued when empty |
| `--turn-ttl` | `600` | Lifetime of ephemeral TURN credentials (seconds) |

Admin pages (Basic Auth):

| Page | Purpose |
|---|---|
| `/admin/devices` | Device list and online status |
| `/admin/grants` | Controller ↔ device grants |
| `/admin/packages` | Package center |
| `/admin/audit` | Audit log |
| `/admin/access` | Enrollment tokens and controller public keys |

### Enroll an agent (one-click install)

1. Open `/admin/access` and generate an enrollment token (15 minutes / 1 hour / 24 hours). You get a one-time token.
2. On the target machine, open PowerShell as administrator:

   ```powershell
   irm "<server>/bootstrap/install.ps1?token=<token>" | iex
   ```

3. The script exchanges the token at `/bootstrap/claim` for a device ID, downloads the agent package, and installs it silently.

Tokens are single-use and expire automatically. Unused tokens can be deleted at any time; deleting a used token returns 404.
Place an `.exe` installer whose filename contains both `rebleach` and `agent` in `--packages-dir`, otherwise
`/bootstrap/agent` returns 404.

### SSH access from the CLI

```powershell
# First run: generate an operator key, register the public key, write the ssh config
rebleach-client --server wss://signal.example.com/ws setup --device <controller_id> --user admin --pass <password>

# Later: re-render the config after grants change
rebleach-client --server wss://signal.example.com/ws ssh-config --controller <controller_id>

# Connect
ssh rbl-<device_id>
```

`setup` will:

- create (or reuse) an operator key at `%APPDATA%\rebleach\ssh\<controller_id>.key`;
- register the public key through `POST /api/v1/ssh-keys`;
- render `~/.ssh/config.d/rebleach.conf` from active grants returned by `GET /api/v1/grants`, and make sure
  `~/.ssh/config` contains `Include ~/.ssh/config.d/*.conf`.

The agent syncs authorized keys from `/agent/ssh-keys` every 60 seconds into `ssh_authorized_keys`, so new grants take
effect within one minute without restarting the service. See the [usage guide](docs/Rebleach-Usage.md) for the full
walkthrough.

---

## Configuration notes (replace as needed for your deployment)

- **Default signaling address**: injected at build time via an environment variable; a placeholder is used when unset:

  ```bash
  REBLEACH_DEFAULT_SERVER=wss://your-server.example.com/ws cargo build --release
  ```

- **Auto-update endpoints**: the endpoints in `tauri.conf.json` / `tauri.agent.conf.json` point to this project's update
  service. Change them for a self-hosted deployment. The update signing key is provided by `TAURI_SIGNING_PRIVATE_KEY`
  at build time and is **never committed**.
- **STUN / TURN**: public STUN is used by default. For production, run your own coturn and issue short-lived credentials
  through the server's `--turn-url` / `--turn-secret`.
- **TLS root certificates**: `app/src-tauri/certs/rebleach-root.crt` and `client/certs/rebleach-root.crt` are **public
  certificates** (used to verify the chain). Replace them with your own CA certificate.
- **Virtual display driver**: usbmmidd is third-party software. The repository only contains the invocation scripts;
  place the driver files yourself. See [virtual-display/README.md](app/src-tauri/resources/virtual-display/README.md).

---

## Notes

- The repository contains no private keys, tokens, or device identifiers. Addresses in the documentation are examples;
  substitute your own for real deployments.
- Device identity keys are generated locally on first run and stored only in the user data directory.

## Documentation

- [Usage guide (English: GUI / CLI / SSH / server enrollment)](docs/Rebleach-Usage.md)
- [使用说明（简体中文）](docs/Rebleach-使用说明.md)

## License

This project is licensed under the [PolyForm Strict License 1.0.0](LICENSE). The source is publicly visible, but this is
**not an OSI-approved open-source license**.

| Action | Allowed |
|---|---|
| Personal study, research, testing, noncommercial use | ✅ Yes |
| Internal use by nonprofits, educational / research / government institutions | ✅ Yes |
| Commercial use, production use inside a company | ❌ Written permission required |
| Copying, distribution, redistribution | ❌ Written permission required |
| Modification, derivative works, building on this project | ❌ Written permission required |

For commercial licensing, derivative development, or distribution, contact the author via GitHub
[@zhema7](https://github.com/zhema7) to obtain written permission.

Third-party components (such as `app/ui/vendor/xterm` and the usbmmidd scripts/drivers) remain under their own licenses
and are not affected by this project's license.

Copyright (c) 2026 zhema7. All rights not expressly granted are reserved.
