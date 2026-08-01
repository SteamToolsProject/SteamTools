![SteamTools](https://socialify.git.ci/SteamToolsProject/SteamTools/image?description=1&descriptionEditable=An%20in-process%20toolbox%20for%20the%20Steam%20client%20on%20Windows%2C%20written%20in%20Rust.&font=Inter&forks=1&issues=1&logo=https%3A%2F%2Fraw.githubusercontent.com%2FSteamToolsProject%2FSteamTools%2Fmaster%2Fassets%2Forg-logo-512.png&language=1&name=1&owner=1&pattern=Diagonal%20Stripes&stargazers=1&theme=Dark)

<div align="center">

[![License](https://img.shields.io/badge/License-GPLv3-blue?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-stable-DEA584?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Windows](https://img.shields.io/badge/Windows-10%2B-0078d6?style=flat-square&logo=windows&logoColor=white)](https://www.microsoft.com/windows)

**English** · [简体中文](docs/README_ZH.md)

**[Quick Start](#usage)** · **[Configuration](#configuration-optional)** · **[Build](#build)** · **[SECURITY](SECURITY.md)**

</div>

> [!Warning]
> For engineering / research purposes only. Follow local laws and the Steam Terms of Service. **Use an alt account for testing.**

---

## About

**SteamTools** is an in-process Rust toolbox for the Steam client on Windows: one-click library import, library context-menu enhancements, an embedded configuration panel, a download kit, and store access acceleration.

Main flow: click "Add to Library" on a store page → fetch metadata through the catalog provider chain → atomically persist to `config/lua/stt_{id}.lua` → refresh / remove from the library. It references the capabilities and Lua ecosystem of [OpenSteamTool](https://github.com/OpenSteam001/OpenSteamTool), but the product design and implementation are independent.

## Features

### Catalog Import `catalog_add` (default on)

- "Add to Library" button on store detail pages: AppId → catalog provider chain (CustomHttp / Lua / community multi-source) → centralized validation → metadata **atomically persisted** to `config/lua/stt_{id}.lua`
- Retryable / degradable failures with consistent feedback across button text, config panel note, and host.log
- Library management: refresh manifest / remove managed apps with one click

### Config Panel `config_ui` (default on)

- Embedded config panel inside Steam (Preact + TSX, single IIFE ~38 KB, no runtime external resources)
- Tool toggles / catalog source / manifest source / log level / Lua directories; changes are written to `steamtools.toml` and hot-reloaded
- Close via Esc / mask click / close button; focus never leaks to the Steam background

### Library UX `library_ux` (default on)

- Right-click on managed apps appends "Refresh manifest / Remove from library"; **the original Steam menu is fully preserved**, SteamTools items are only appended at the bottom
- Unmanaged apps are never intercepted

### Download Kit `download_kit` (default on)

- Four independent capabilities: manifest pinning (GID/size), depot key, access token, manifest request code
- Hooks attach only on **exact SHA + entry signature match**; a missing symbol / pattern degrades that capability alone
- Each capability has an independent runtime switch and tool-center status (Ready / DataMissing / PatternMissing / EnvDisabled)

### Store Acceleration `store_accel` (default off)

- Local DNS/CDN candidate optimization: UDP public DNS, independent A/AAAA caches, TCP + SNI/cert/HEAD probing, candidate cooldown and health cache
- Optional user-owned Clash loopback fallback (at most one retry after local candidates fail); `http_connect` kept as an optional user-relay egress
- PAC save & rollback; **no root cert install, no HTTPS MITM, no Hosts writes, no WinDivert**; `egress = "disabled"` leaves the system proxy untouched

## Usage

1. Build (requires Windows 10+, Rust stable MSVC toolchain):

   ```powershell
   cargo test --workspace --all-features --no-fail-fast -- --test-threads=1
   cargo build -p stt-host -p stt-store-accel -p stt-loader-dwmapi -p stt-loader-xinput --release
   ```

2. Copy the 3 artifacts to the **Steam root** (e.g. `C:\Program Files (x86)\Steam\`):

   | Artifact | Description |
   |---|---|
   | `SteamTools.dll` | Host: config / state / all capabilities (includes the embedded store-accel thread; no separate helper) |
   | `dwmapi.dll` | Loader (pure-Rust forwarding + host loading) |
   | `xinput1_4.dll` | Alternate loader channel (same) |

3. Restart Steam and check `steamtools/host.log` for `status=init complete`; the "Add to Library" button appears on store detail pages and a SteamTools entry appears in the nav bar.

> [!NOTE]
> A local WinHTTP test is known to be flaky; rerunning it passes. It does not affect the build.

## Configuration (optional)

Place `steamtools.toml` in the Steam root (next to `steam.exe`). If absent, built-in defaults are used (four tools on by default). The file is watched and hot-reloaded:

```toml
[tools.enabled]
catalog_add = true        # catalog import
library_ux = true         # library UX
config_ui = true          # config panel
download_kit = true       # download kit (default on)
store_accel = false       # store accel (default off)

[catalog]
mode = "disabled"         # disabled / custom_http / lua / community / mock
url_template = ""         # for custom_http: https://example.com/catalog/{app_id}

[manifest]
url = "opensteamtool"     # manifest request code source: opensteamtool / steamrun / wudrm

[store_accel]
egress = "disabled"       # disabled / direct_dns / local_cdn / http_connect
clash_fallback = "127.0.0.1:7890"   # optional; loopback only, at most one retry after local candidates fail
```

## Steam version compatibility

- No hardcoded offsets: on every launch, the SHA-256 of `steamclient64.dll` / `steamui.dll` is matched against external patterns
- Patterns are fetched from `SteamTools-Patterns` first (atomically cached per DLL SHA at `steamtools/pattern/{component}/{sha}.toml`); when the remote is unavailable or mismatched, fall back to the local cache and the `opensteamtool/pattern` legacy path
- A missing pattern disables only the affected capability (UI contribution / hook); everything else keeps working, and host.log / the tool center state the degradation reason

## Build

MSVC release artifacts:

| Artifact | Output path |
|---|---|
| `SteamTools.dll` | `target\release\steamtools.dll` (`stt-host` cdylib output name) |
| `dwmapi.dll` | `target\release\dwmapi.dll` (`stt-loader-dwmapi`) |
| `xinput1_4.dll` | `target\release\xinput1_4.dll` (`stt-loader-xinput`) |

The config-panel bundle is built separately (the artifact is committed; a plain `cargo build` never invokes Node):

```powershell
cd crates\stt-steamui\ui
npm ci && npm run check
```

## Architecture

```text
dwmapi.dll / xinput1_4.dll        # pure-Rust loaders → LoadLibrary(SteamTools.dll)
        └→ SteamTools.dll (stt-host)
             ├─ stt-config         TOML + Lua config, hot reload, atomic persistence
             ├─ stt-core           import state / single source of truth for AppRules
             ├─ stt-catalog        catalog provider chain (multi-source, centralized validation)
             ├─ stt-steamui        CDP injection: import button / config panel / library context menu
             ├─ stt-steamclient    ownership / manifest / key / token / request code
             └─ stt-store-accel    store acceleration (in-DLL thread, PAC + CONNECT)
```

Companion crates: `stt-metadata` (pattern parsing) · `stt-hook` (detachable detours) · `stt-platform` (restricted WinHTTP / filesystem).

## Boundaries

- **"Import" means metadata persistence**, not download completion; download capabilities live in the separate `download_kit`, on by default and disableable from the config panel
- `store_accel` only handles store / community / workshop web traffic; it never proxies downloads, game traffic, P2P, or tickets
- Capabilities drift with Steam versions: a stale pattern degrades that capability alone

## Disclaimer

1. **Research use** — for engineering / research / learning only; no commercial, abusive, or otherwise unauthorized use
2. **Account risk** — import and download-kit capabilities may violate the Steam Terms of Service and carry account risk; **strongly use an alt account for testing**; any bans or losses are the user's own responsibility
3. **Not affiliated** — this project has no affiliation with Valve / Steam and is not an official product
4. **Data & privacy** — no account data, keys, or tickets are uploaded; logs are written locally to `steamtools/host.log` and redacted
5. **No warranty** — use at your own risk

> [!CAUTION]
> Using this project means you accept the terms above.

## Credits

- [OpenSteamTool](https://github.com/OpenSteam001/OpenSteamTool) — capability and Lua-ecosystem reference; product design and implementation are independent
- [Steamcommunity 302](https://www.dogfight360.com/blog/18682/) - store & community unlocking ideas
- [Feather Icons](https://feathericons.com/) (MIT) - base of the logo import mark

## License

Licensed under [GPLv3](./LICENSE). Security reports: [SECURITY.md](SECURITY.md).



