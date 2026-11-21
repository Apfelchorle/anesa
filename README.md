# Hayase Tauri

Tauri v2 + Rust migration of Hayase away from Electron, aimed at 1:1 core functionality for the existing `hayase-interface` with hardened security defaults.

> **Status:** core shell and an in-process Rust torrent backend are implemented for the main playback/library flows. Several advanced Electron/Node features remain phased (see below).

## Implemented

### Core shell / app behavior
- **Tauri v2 Rust shell** with plugin wiring and capability manifest.
- **Frameless main window** created in Rust with a strict **navigation guard**: internal origins load in the privileged webview; allowlisted external links open in the system browser; other navigations are blocked.
- **System tray** (show/hide, click-to-toggle, quit/update placeholders) and **hide-to-tray** close behavior.
- **Single-instance** handling with deep-link forwarding to the existing window.
- **Deep links (`hayase://`)**: strict parsing, length cap, credential rejection, route allowlist, `devtools` debug-only, `authorize` forwarding.
- **Window/app controls**: minimise, maximise/toggle, fullscreen, close, focus, restart, zoom clamp.
- **Settings persistence**: typed `ClientSettings` + app settings, atomic JSON writes (temp + fsync + rename), mutex-serialized updates, corruption tolerance, best-effort legacy Electron `settings.json` import/migration.
- **Player/download selection**: native dialogs with validation; download dir is propagated to the torrent backend.
- **External player spawning**: validated loopback HTTP URL + validated executable, previous player killed, no shell, Flatpak host spawn support, child-process cleanup on exit.
- **Open URL**: strict HTTPS host/path allowlist (fixes prefix-confusion issues).
- **Logs/device info/version/accent/update commands** scaffolded (tail-only log read; device info via `sysinfo`).

### Torrent engine (Rust, in-process)
- **Engine:** [`librqbit 9.x`](https://crates.io/crates/librqbit) session managed from Tauri async runtime; falls back to a degraded stub if startup fails so the app remains launchable.
- **Add/play:** magnet URI, infohash, remote torrent URL, or raw `.torrent` bytes → resolves metadata and returns `TorrentFile[]` with stream URLs.
- **Local streaming server:** axum server bound to `127.0.0.1:<ephemeral>` with a random per-session token in every URL, loopback-only, **HTTP Range** support for seeking, content-type/length/disposition headers, CORS for the webview, and file access by torrent hash + file id (no filesystem paths in URLs).
- **Stats / inspection:** `active_torrents`, `torrent_info`, `file_info` (size/progress/selection), `peer_info` (best-effort mapping from librqbit peer snapshots), `trackers` (tracker URLs; scrape stats not yet populated), `protocol_status`.
- **Library / cache:** atomic JSON library index + per-hash `.torrent` cache with validated hashes (no path traversal), persisted metadata on add, `library`, `cached_torrents`, `rescan_torrents` (best-effort via cached `.torrent` bytes), `delete_torrents` (removes data + cache/library), `remove_background_torrents`.
- **Settings:** download/upload rate limits and peer limit applied from `ClientSettings`; DHT can be disabled via `torrentDHT` flag (matching Electron semantics); download directory changes persisted/applied for future torrents.
- **System checks:** `check_available_space` (disk lookup via `sysinfo`); `check_incoming_connections` currently returns a conservative result (full NAT/DHT reachability parity not yet implemented).
- **Placeholders:** NZB/HTTP webseeds, peer-count scrape, remote host, Chromecast/DLNA, DoH wiring.

### Native bridge (`bridge/native-bridge.ts`)
- Installs `window.native` in Tauri (`__TAURI_INTERNALS__`) by mapping to explicit `invoke` commands and `native://*` events; preserves the `hayase-interface` contract (including `addTorrent(id, mediaId, episode, background)`, `updateSettings(ClientSettings)`, `selectDownload(kind)`, `activeTorrents`, `cachedTorrents`, `createNZB(hash,url)`, `createHTTPWebSeed(hash,url,authorization,fileIndex,rateLimit)`, etc.).
- Falls back to the interface's built-in dummy implementation outside Tauri.

### Security hardening
- **Strict URL validation** (`src-tauri/src/security.rs`): exact host allowlist, HTTPS-only, no credentials, path rules for shared hosts (e.g. GitHub sponsors only), rejects prefix-confusion hosts.
- **Player URL validation**: `http` only, loopback only, explicit port, no credentials.
- **Remote fetch / torrent URL validation**: http(s) only, credentials rejected, literal loopback/private/link-local/multicast IPs blocked (SSRF defense).
- **Path validation**: absolute paths, no traversal components/NUL, existing directory checks, root-directory guard; torrent cache/library hashes validated before filesystem use.
- **Torrent identifier validation**: 40/64-char hex infohashes or `magnet:?xt=urn:btih:` only.
- **Streaming isolation**: loopback bind, per-session random token, no path-based file access, Range clamping.
- **Navigation guard + CSP + `freezePrototype`**; CSP allows local streaming from `http://127.0.0.1:*`/`http://localhost:*`.
- **No broad API exposure**: bridge invokes explicit commands only.
- **Atomic serialized settings/lib writes**, tail-only log reads.
- **Unit tests** for URL/player/torrent-identifier validation and settings/library roundtrips (`cargo test`).

## Not yet fully ported / parity gaps

- **NZB manager and HTTP webseed peer integration** (commands exist but return "not implemented").
- **EBML/media extraction:** subtitles, chapters, fonts/attachments, thumbnail/metadata parsing, and attachment serving.
- **Tracker scrape / accurate tracker stats / `updatePeerCounts` parity.**
- **Incoming-connectivity test parity** (Electron used DHT announce/NAT checks); current implementation is conservative.
- **Advanced torrent behaviors:** full Electron/WebTorrent parity for streamed-only piece prioritization, session/background semantics, PEX/runtime toggles, bandwidth scheduling, port/DHT rebind semantics, `remoteAccess`/host server, DoH resolver wiring.
- **Casting:** Chromecast/DLNA discovery/control.
- **Auth popups/flows (AL/MAL)**, **extension/plugin system**, **Discord RPC**, **OS media session integration**, **CORS/session header behavior**, **real updater flow/signing**, **OS accent/GPU feature parity**.
- **Frontend bundling/integration:** Tauri currently points at the scaffold `dist`; wire `devUrl`/`frontendDist` (or build pipeline) to `hayase-interface` and load `bridge/native-bridge.ts` before interface bootstrap for end-to-end UI parity testing.

## Project layout

```text
hayase-tauri/
  README.md
  package.json                 # frontend/tooling placeholder
  index.html                   # minimal placeholder frontend
  src/                         # placeholder frontend source
  bridge/
    native-bridge.ts           # Tauri invoke/event -> window.native compatibility shim
  src-tauri/
    Cargo.toml
    tauri.conf.json
    capabilities/default.json
    icons/
    src/
      main.rs                  # desktop entrypoint
      lib.rs                   # app builder, engine init, window/nav guard, invoke handlers
      commands.rs              # Tauri command handlers (native API surface)
      events.rs                # event name constants
      security.rs              # URL/path/player/torrent-id validation + tests
      settings.rs              # atomic persisted settings + Electron migration + tests
      protocol.rs              # hayase:// deep-link parsing/routing
      tray.rs                  # system tray
      player.rs                # external player process tracking + cleanup
      torrent/
        mod.rs                 # native-shaped types, ClientSettings, backend trait/state
        rqbit.rs               # librqbit-backed engine implementation
        stream_server.rs       # loopback tokenized Range streaming server
        library.rs             # atomic library index + .torrent cache
```

## Building

Prerequisites: Rust stable, Node/pnpm, and [Tauri system dependencies](https://v2.tauri.app/start/prerequisites/) (WebKit2GTK/GTK on Linux, etc.).

```bash
# from hayase-tauri/
pnpm install
pnpm build
cd src-tauri
cargo check
cargo test
# run app (after pointing frontend at hayase-interface build/dev server as needed)
cargo tauri dev
```

Notes:
- Icons are generated in `src-tauri/icons/` (regenerate with `pnpm tauri icon <png>` if desired).
- This workspace has `pnpm-workspace.yaml` with `onlyBuiltDependencies: [esbuild]` for pnpm v12 build-script approval.
- `librqbit` is used as the default torrent engine; the app starts with a degraded stub backend if engine initialization fails (errors logged).
