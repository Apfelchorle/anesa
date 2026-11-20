/**
 * Tauri -> Hayase `window.native` compatibility bridge.
 *
 * Purpose: allow the existing `hayase-interface` (which merges/uses `globalThis.native`)
 * to run against the Tauri/Rust backend with minimal UI changes during migration.
 *
 * Usage (early in app bootstrap, before modules read `native`):
 *   import { installNativeBridge } from './bridge/native-bridge'
 *   await installNativeBridge()
 *
 * Notes:
 * - Tauri v2 uses `@tauri-apps/api/core` (`invoke`) and `@tauri-apps/api/event` (`listen`).
 * - Command names and payload shapes must match `src-tauri/src/commands.rs`.
 * - Security: only invoke explicit commands; do not expose Tauri internals to page scripts.
 */

import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

type AnyFn = (...args: any[]) => any

const w = window as any

interface NavigatePayload {
  target: string
  value?: string | null
}

interface NativeEventHandlers {
  navigate?: (payload: NavigatePayload) => void
  error?: (payload: unknown) => void
  updateProgress?: (percent: number) => void
}

const handlers: NativeEventHandlers = {}

async function setupEventBridges() {
  await listen<NavigatePayload>('native://navigate', (event) => {
    handlers.navigate?.(event.payload)
  })

  await listen<unknown>('native://error', (event) => {
    handlers.error?.(event.payload)
  })

  await listen<number>('native://update-progress', (event) => {
    handlers.updateProgress?.(event.payload)
  })
}

function isTauri(): boolean {
  return typeof (w as any).__TAURI_INTERNALS__ !== 'undefined'
}

export async function installNativeBridge() {
  if (!isTauri()) {
    // Browser/dev fallback: keep the interface's built-in dummy native implementation.
    return w.native
  }

  await setupEventBridges()

  const native = {
    // --- app/window ---
    isApp: true,
    version: () => invoke<string>('version'),
    openURL: (url: string) => invoke('open_url', { url }),
    minimise: () => invoke('window_minimise'),
    maximise: () => invoke('window_maximise'),
    fullscreen: (enabled: boolean) => invoke('window_fullscreen', { enabled }),
    close: () => invoke('window_close'),
    focus: () => invoke('window_focus'),
    restart: () => invoke('app_restart'),
    quit: () => invoke('app_quit'),
    setZoom: (scale: number) => invoke('set_zoom', { scale }),
    setHideToTray: (enabled: boolean) => invoke('set_hide_to_tray', { enabled }),

    selectPlayer: () => invoke<string | null>('select_player'),
    selectDownload: (kind?: 'cache' | 'internal' | 'sdcard') =>
      invoke<string | null>('select_download', { kind }),
    spawnPlayer: (url: string) => invoke('spawn_player', { url }),

    // --- settings ---
    getSettings: () => invoke('get_settings'),
    updateSettings: (settings?: unknown) => invoke('update_settings', { settings }),
    setDOH: (dns: string) => invoke('set_doh', { dns }),
    setAngle: (angle: string) => invoke('set_angle', { angle }),

    // --- misc ---
    getLogs: () => invoke<string>('get_logs'),
    getDeviceInfo: () => invoke('get_device_info'),
    defaultTransparency: () => false,
    accentColor: () => invoke<string>('accent_color'),

    // --- updates ---
    checkUpdate: () => invoke<boolean>('check_update'),
    updateReady: () => invoke('update_ready'),
    updateAndRestart: () => invoke('update_and_restart'),

    // --- event subscriptions (mirrors Electron IPC/preload behavior) ---
    navigate: (cb: (payload: NavigatePayload) => void) => {
      handlers.navigate = cb
    },
    errors: (cb: (payload: unknown) => void) => {
      handlers.error = cb
    },
    updateProgress: async (cbOrPercent?: number | ((percent: number) => void)) => {
      if (typeof cbOrPercent === 'function') {
        handlers.updateProgress = cbOrPercent
      } else if (typeof cbOrPercent === 'number') {
        await invoke('torrent_download_progress', { percent: cbOrPercent })
      }
    },

    // --- torrent backend ---
    addTorrent: (id: unknown, mediaID?: number, episode?: number, background?: boolean) =>
      invoke('add_torrent', {
        id,
        media_id: mediaID ?? 0,
        episode: episode ?? 0,
        background: !!background
      }),
    activeTorrents: () => invoke('torrent_active'),
    torrentInfo: async (hash?: string) => {
      if (!hash) {
        const active = await invoke<any[]>('torrent_active')
        return active?.[0] ?? {
          hash: '',
          name: '',
          progress: 0,
          size: { total: 0, downloaded: 0, uploaded: 0 },
          speed: { down: 0, up: 0 },
          time: { remaining: 0, elapsed: 0 },
          peers: { seeders: 0, leechers: 0, wires: 0 },
          pieces: { total: 0, size: 0 }
        }
      }
      return invoke('torrent_info', { hash })
    },
    fileInfo: (hash: string) => invoke('torrent_file_info', { hash }),
    peerInfo: (hash: string) => invoke('torrent_peer_info', { hash }),
    trackers: (hash: string) => invoke('torrent_trackers', { hash }),
    protocolStatus: (hash: string) => invoke('torrent_protocol_status', { hash }),
    library: () => invoke('torrent_library'),
    cachedTorrents: () => invoke('torrent_cached'),
    rescanTorrents: (hashes: string[] = []) => invoke('torrent_rescan', { hashes }),
    deleteTorrents: (hashes: string[]) => invoke('torrent_delete', { hashes }),
    removeBackgroundTorrents: (hashes: string[]) =>
      invoke('torrent_remove_background', { hashes }),
    checkAvailableSpace: (path?: string) => invoke('torrent_check_available_space', { path }),
    checkIncomingConnections: (port = 0) =>
      invoke<boolean>('torrent_check_incoming_connections', { port }),
    createNZB: (hash: string, url: string) => invoke('torrent_create_nzb', { hash, url }),
    createHTTPWebSeed: (
      hash: string,
      url: string,
      authorization?: string,
      fileIndex?: number,
      rateLimit?: number
    ) =>
      invoke('torrent_create_http_webseed', {
        hash,
        url,
        authorization,
        file_index: fileIndex,
        rate_limit: rateLimit
      }),

    // --- media pipeline / devtools / misc ---
    attachments: (hash: string, id: number) => invoke('torrent_attachments', { hash, id }),
    tracks: (hash: string, id: number) => invoke('torrent_tracks', { hash, id }),
    subtitles: async (_hash: string, _id: number, _cb?: AnyFn) => invoke('torrent_subtitles', { hash: _hash, id: _id }),
    chapters: (hash: string, id: number) => invoke('torrent_chapters', { hash, id }),
    updatePeerCounts: (hashes: string[]) => invoke('torrent_update_peer_counts', { hashes }),
    downloadProgress: undefined as any, // replaced below to support both call styles

    openUIDevtools: () => invoke('open_ui_devtools'),
    openTorrentDevtools: () => invoke('open_torrent_devtools'),
    debug: (levels?: string) => invoke('debug', { levels: levels ?? '' }),
    profile: (seconds = 0) => invoke('profile', { seconds }),

    // --- placeholders preserving API shape; implement progressively ---
    getDisplays: async () => [],
    castPlay: async () => undefined,
    castClose: async () => undefined,
    share: async (...args: any[]) => (navigator as any).share?.(...args),
    authAL: async (_url: string) => {
      throw new Error('authAL bridge not implemented yet')
    },
    authMAL: async (_url: string) => {
      throw new Error('authMAL bridge not implemented yet')
    },
    pluginList: async () => [],
    pluginImport: async () => {
      throw new Error('Plugins not implemented in Tauri scaffold')
    },
    pluginDelete: async () => undefined,
    pluginPopup: async () => undefined,
    enableCORS: async () => undefined,
    setExperimentalGPU: async () => undefined,
    transparency: async () => undefined,
    toggleDiscordDetails: async () => undefined,
    setMediaSession: async () => undefined,
    setPositionState: async () => undefined,
    setPlayBackState: async () => undefined,
    setActionHandler: async () => undefined,
    unsafeUseInternalALAPI: async () => undefined,
    updateToNewEndpoint: async () => undefined
  }

  // Support both `downloadProgress(cb)` (subscribe) and `downloadProgress(percent)` (report).
  native.downloadProgress = async (cbOrPercent?: number | ((percent: number) => void)) => {
    if (typeof cbOrPercent === 'function') {
      handlers.updateProgress = cbOrPercent
      return
    }
    if (typeof cbOrPercent === 'number') {
      await invoke('torrent_download_progress', { percent: cbOrPercent })
    }
  }

  // Merge over the interface's defaults; explicit native methods win.
  w.native = Object.assign({}, w.native, native)
  return w.native
}
