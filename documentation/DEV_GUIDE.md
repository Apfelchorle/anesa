# Developer Guide

## New developers — start here

1. Read [`ARCHITECTURE.md`](ARCHITECTURE.md) for the system overview.
2. Read [`FILES.md`](FILES.md) for the file index.
3. Set up the dev environment (see README Quick start).
4. Run `cargo tauri dev` and open the app.
5. Pick a small issue; ask in the repo's discussions.

### Key concepts

- **Backend = Rust (Tauri 2)**. All torrent/media logic lives in `src-tauri/src`. Commands are declared in `commands.rs` and registered in `lib.rs`.
- **Frontend = SvelteKit**. Pages in `interface/src/routes`, shared logic in `interface/src/lib`.
- **Bridge**: `bridge/native-bridge.ts` (Electron compat) and `src-tauri/init-bridge.js` (injected). The latter is the active one.
- **Torrent backend trait**: `TorrentBackend` in `torrent/mod.rs`. `RqbitBackend` implements it; `StubTorrentBackend` is the degraded fallback.
- **Streaming**: media is served over `127.0.0.1:{port}/stream/{token}/{hash}/{file_id}/{filename}` with Range support.

### Common tasks

| Task | Where |
|------|-------|
| Add a Tauri command | `src-tauri/src/commands.rs` + register in `lib.rs` |
| Add a frontend page | `interface/src/routes/app/.../+page.svelte` |
| Add a settings field | `settings.rs` + settings UI |
| Touch torrent logic | `torrent/rqbit.rs` + `torrent/mod.rs` trait |
| Touch subtitles | `torrent/subtitles.rs`, `torrent/ebml.rs`, `media.rs`, frontend `subtitles.ts` |

### Code style

- Rust: `cargo fmt`, `cargo clippy`. Errors via `Result<T, String>`.
- TS/Svelte: `pnpm lint`, `pnpm check`. Prefer existing patterns (stores, `native.*`).
- Commits: conventional-ish; use `[skip ci]` for doc-only changes.

## Senior developers — reference

### Invariants

- `validate_infohash` / `validate_*` must be called in every command touching user input.
- `TorrentBackend` is `Send + Sync`; implementations must be too.
- `ensure_media_meta` is per-file locked; do not open parallel streams for the same file.
- `SubtitleTapRegistry` uses a `std::sync::Mutex`; never hold it across `.await`.
- Frontend `native.subtitles` is registered once per (hash, id); backend dedups via `claim_extractor`.

### Subtitle pipeline (post-fix)

```
player reads bytes ──► stream_server TapReader ──► SubtitleTapRegistry
                                                          │
                                                          ▼
                                            run_subtitle_extractor
                                            (metadata via ranged read,
                                             then sequential full-file read)
                                                          │
                                                          ▼
                                            publish_cues ──► cue_tx (broadcast)
                                                          │
                                                          ▼
                                            torrent_subtitles command
                                            ──► emit('torrent://subtitle')
                                                          │
                                                          ▼
                                            frontend processChunk (ms)
```

### Pitfalls

- `matroska::Matroska::open` follows SeekHead; on partial files it can fail. Prefer stream reads.
- `TimestampScale` is ns/tick (default 1_000_000 = ms). Cue time = `tick * scale / 1e6`.
- `matroska_demuxer` is no longer used; do not reintroduce.
- Frontend `processChunk` expects **milliseconds**, not seconds.
- CORS: external API calls from the webview may be blocked; use the Rust proxy when needed.

### Release

- `.github/workflows/release.yml` builds Tauri bundles.
- Use `[skip ci]` for doc-only commits to avoid redundant builds.

### Debugging

- `RUST_LOG=debug cargo tauri dev` for backend logs.
- Frontend: browser devtools in the webview (`open_ui_devtools`).
- Subtitle issues: check `torrent_subtitles` logs and cue `time`/`duration` values.
