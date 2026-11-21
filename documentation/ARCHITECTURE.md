# Architecture

## Overview

Anesa is a Tauri 2 desktop app. The **Rust backend** (`src-tauri/`) owns the torrent engine, streaming server, and media parsing. The **SvelteKit frontend** (`interface/`) renders the UI and talks to the backend over Tauri IPC + a loopback HTTP stream server.

```
┌────────────────────────────┐         Tauri IPC          ┌────────────────────────────┐
│  Frontend (SvelteKit)      │  ── invoke('torrent_*') ──►│  Rust backend (Tauri 2)    │
│  interface/src             │  ◄── emit('torrent://…') ──│  src-tauri/src             │
│                            │                            │                            │
│  native.* (bridge)         │  HTTP Range (127.0.0.1)    │  librqbit Session          │
│  player <video src=url>    │  ────────────────────────► │  stream_server (axum)      │
└────────────────────────────┘                            └────────────────────────────┘
```

## Backend modules (`src-tauri/src`)

| Module | Responsibility |
|--------|----------------|
| `commands.rs` | Tauri command surface; validates args; emits events. |
| `lib.rs` | App setup, plugin registration, window, deep-link. |
| `events.rs` | Event name constants (`torrent://subtitle`, etc.). |
| `player/` | Player state. |
| `security/` | Path/URL/hash validation. |
| `settings/` | App settings persistence. |
| `torrent/mod.rs` | `TorrentBackend` trait, shared types, stub backend. |
| `torrent/rqbit.rs` | librqbit engine implementation. |
| `torrent/media.rs` | MKV metadata parsing (disk + stream). |
| `torrent/ebml.rs` | EBML primitives + TimestampScale probe. |
| `torrent/subtitles.rs` | Live EBML subtitle extractor + tap registry. |
| `torrent/stream_server.rs` | Loopback HTTP streaming with Range + subtitle tap. |
| `torrent/library.rs` | Library persistence. |
| `proxy.rs` | W2G / proxy helpers. |

## Frontend modules (`interface/src`)

| Area | Responsibility |
|------|----------------|
| `routes/app/*` | Pages: home, player, client/*, settings/*, chat, w2g, search, schedule, profile. |
| `routes/setup/*` | First-run setup wizard. |
| `lib/modules/torrent` | Torrent client UI state & API. |
| `lib/modules/anilist` | AniList GraphQL client. |
| `lib/modules/auth` | OAuth flows. |
| `lib/modules/extensions` | Extension system. |
| `lib/modules/irc` | Global IRC chat. |
| `lib/modules/w2g` | Watch Together P2P. |
| `lib/modules/settings` | Settings store. |
| `lib/components/ui/player` | Player + subtitles (JASSUB). |
| `lib/components/ui/torrentclient` | Torrent client UI. |

## Key data flows

### Media playback
1. Frontend resolves a file → `mediaInfo.file.url` = `http://127.0.0.1:{port}/stream/...`.
2. `<video src=url>` (or MediaBunny) issues Range requests.
3. `stream_server::stream_handler` serves bytes via `open_file_stream` (librqbit).
4. `TapReader` tees every byte into `SubtitleTapRegistry`.

### Embedded subtitles
1. `native.subtitles(hash, id)` → `torrent_subtitles` command.
2. Backend spawns `run_subtitle_extractor`: parses metadata (stream), then reads the whole file sequentially, emitting cues via `publish_cues`.
3. Cues arrive at frontend as `torrent://subtitle` events → `processChunk`.

### Watch Together
1. Host creates lobby → P2P swarm via `p2pt`.
2. Peers receive `media` event → `server.playHash` → backend adds torrent + plays.
3. Host broadcasts `player` state; guests sync with 2s tolerance.

### Global chat
1. Frontend connects to `irc.hybridirc.com:7002` (TLS) via `@thaunknown/web-irc`.
2. Channel `#hayase-4e63ad915`. AniList-linked identity.

## File index

See [`FILES.md`](FILES.md) for the complete per-file listing.
