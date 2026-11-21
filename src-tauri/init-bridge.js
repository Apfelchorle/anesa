(function () {
  if (typeof window === 'undefined') return
  var internals = window.__TAURI_INTERNALS__
  var tauri = window.__TAURI__
  if (!internals && !tauri) return

  function invoke(cmd, args) {
    if (tauri && tauri.core && tauri.core.invoke) return tauri.core.invoke(cmd, args || {})
    if (internals && internals.invoke) return internals.invoke(cmd, args || {})
    return Promise.reject(new Error('Tauri invoke unavailable'))
  }

  // --- Narrow CORS proxy for first-party APIs -------------------------------
  // The webview origin (`http://tauri.localhost` on Windows) is not accepted by
  // several metadata APIs, and some of them send no CORS headers at all. Route
  // only these exact hosts through the Rust command, which enforces the same
  // allowlist. Everything else keeps using the native fetch implementation.
  //
  // Keep this list in sync with ALLOWED_PROXY_HOSTS in src-tauri/src/proxy.rs
  // and the extension worker shim in interface/src/lib/modules/extensions/worker.ts.
  var PROXY_HOSTS = {
    'graphql.anilist.co': true,
    'anilist.co': true,
    'hayase.ani.zip': true,
    'api.animethemes.moe': true,
    'api.aniskip.com': true,
    'api.trace.moe': true,
    'api.myanimelist.net': true,
    'myanimelist.net': true,
    'api.simkl.com': true,
    'kitsu.app': true,
    // First-party extension hosts (trusted extensions only).
    'nekobt.to': true,
    'feed.animetosho.net': true,
    'amenzb.moe': true,
    'aninzb.moe': true,
    'api.tsukihime.org': true,
    'storage.tsukihime.org': true,
    'nyaa.si': true,
    'releases.moe': true,
    'subsplease.org': true,
    'raw.githubusercontent.com': true
  }

  var nativeFetch = typeof window.fetch === 'function' ? window.fetch.bind(window) : null

  function bytesToBase64(bytes) {
    var binary = ''
    var chunk = 0x8000
    for (var i = 0; i < bytes.length; i += chunk) {
      binary += String.fromCharCode.apply(null, bytes.subarray(i, i + chunk))
    }
    return btoa(binary)
  }

  function base64ToBytes(encoded) {
    var binary = atob(encoded)
    var bytes = new Uint8Array(binary.length)
    for (var i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i)
    return bytes
  }

  function isProxiedUrl(url) {
    return url.protocol === 'https:' && PROXY_HOSTS[url.hostname] === true
  }

  function encodeProxyBody(body, headers) {
    if (body == null) return Promise.resolve(null)
    if (typeof body === 'string') {
      if (!headers.has('content-type')) headers.set('content-type', 'text/plain;charset=UTF-8')
      return Promise.resolve(new TextEncoder().encode(body))
    }
    if (typeof URLSearchParams !== 'undefined' && body instanceof URLSearchParams) {
      if (!headers.has('content-type')) headers.set('content-type', 'application/x-www-form-urlencoded;charset=UTF-8')
      return Promise.resolve(new TextEncoder().encode(body.toString()))
    }
    if (typeof Blob !== 'undefined' && body instanceof Blob) {
      return body.arrayBuffer().then(function (buffer) { return new Uint8Array(buffer) })
    }
    if (body instanceof ArrayBuffer) return Promise.resolve(new Uint8Array(body))
    if (ArrayBuffer.isView(body)) return Promise.resolve(new Uint8Array(body.buffer, body.byteOffset, body.byteLength))
    // FormData/ReadableStream cannot cross the IPC boundary faithfully.
    return Promise.resolve(undefined)
  }

  function proxyResponse(data, url) {
    var body = null
    if (data.status !== 204 && data.status !== 205 && data.status !== 304 && data.body_base64) {
      body = base64ToBytes(data.body_base64)
    }
    var response = new Response(body, {
      status: data.status,
      statusText: data.status_text || '',
      headers: data.headers || {}
    })
    try {
      Object.defineProperty(response, 'url', { value: url, configurable: true })
    } catch (e) {}
    return response
  }

  function proxiedFetch(input, init) {
    init = init || {}
    var request = null
    try {
      if (typeof Request !== 'undefined' && input instanceof Request) request = input
    } catch (e) {}

    var rawUrl = request ? request.url : String(input)
    var url
    try {
      url = new URL(rawUrl, window.location.href)
    } catch (e) {
      return nativeFetch(input, init)
    }
    if (!isProxiedUrl(url)) return nativeFetch(input, init)

    var method = String(init.method || (request && request.method) || 'GET').toUpperCase()
    var headers = new Headers(request ? request.headers : undefined)
    if (init.headers) {
      new Headers(init.headers).forEach(function (value, key) { headers.set(key, value) })
    }

    var body = init.body
    if (body === undefined && request && method !== 'GET' && method !== 'HEAD' && request.body && !request.bodyUsed) {
      body = request.arrayBuffer()
    }

    var signal = init.signal || (request && request.signal)
    if (signal && signal.aborted) {
      return Promise.reject(new DOMException('The operation was aborted.', 'AbortError'))
    }

    return Promise.resolve(body)
      .then(function (resolvedBody) { return encodeProxyBody(resolvedBody, headers) })
      .then(function (encoded) {
        if (encoded === undefined) return nativeFetch(input, init)

        var headerObject = {}
        headers.forEach(function (value, key) { headerObject[key] = value })

        var args = {
          request: {
            url: url.toString(),
            method: method,
            headers: headerObject,
            body_base64: encoded ? bytesToBase64(encoded) : null
          }
        }

        var pending = invoke('proxy_fetch', args).then(function (data) {
          return proxyResponse(data, url.toString())
        })

        if (!signal) return pending

        return new Promise(function (resolve, reject) {
          var onAbort = function () {
            reject(new DOMException('The operation was aborted.', 'AbortError'))
          }
          signal.addEventListener('abort', onAbort, { once: true })
          pending.then(function (response) {
            signal.removeEventListener('abort', onAbort)
            resolve(response)
          }, function (error) {
            signal.removeEventListener('abort', onAbort)
            reject(error)
          })
        })
      })
  }

  if (nativeFetch) {
    window.fetch = function (input, init) { return proxiedFetch(input, init) }
  }

  var handlers = {
    navigate: null,
    error: null,
    updateProgress: null,
    downloadProgress: null
  }

  // Subtitle callbacks keyed by `${hash}:${id}`; values are arrays of callbacks.
  var subtitleHandlers = Object.create(null)

  // Pending auth promises resolved by hayase://authorize deep-link navigation events.
  var pendingAuthAL = []
  var pendingAuthMAL = []

  function parseHashParams(str) {
    var out = {}
    if (!str) return out
    var s = String(str)
    var idx = s.indexOf('#')
    if (idx >= 0) s = s.slice(idx + 1)
    idx = s.indexOf('?')
    if (idx >= 0 && s.indexOf('=') > idx) s = s.slice(idx + 1)
    s.replace(/^\?/, '').split('&').forEach(function (part) {
      if (!part) return
      var kv = part.split('=')
      var k = decodeURIComponent(kv[0] || '')
      var v = decodeURIComponent((kv.slice(1).join('=')) || '')
      if (k) out[k] = v
    })
    return out
  }

  function parseQueryParams(str) {
    var out = {}
    if (!str) return out
    var s = String(str)
    var q = s.indexOf('?')
    if (q >= 0) s = s.slice(q + 1)
    var h = s.indexOf('#')
    if (h >= 0) s = s.slice(0, h)
    s.split('&').forEach(function (part) {
      if (!part) return
      var kv = part.split('=')
      var k = decodeURIComponent(kv[0] || '')
      var v = decodeURIComponent((kv.slice(1).join('=')) || '')
      if (k) out[k] = v
    })
    return out
  }

  function handleNavigate(payload) {
    payload = payload || {}
    if (payload.target === 'authorize') {
      var value = payload.value || ''
      // AniList implicit flow: token in fragment (#access_token=...)
      if (value.indexOf('access_token') !== -1 || value.indexOf('#access_token') !== -1) {
        var hp = parseHashParams(value)
        if (hp.access_token) {
          var res = {
            access_token: hp.access_token,
            expires_in: hp.expires_in || '',
            token_type: hp.token_type || 'Bearer'
          }
          var resolvers = pendingAuthAL.splice(0)
          if (resolvers.length) {
            resolvers.forEach(function (r) { try { r.resolve(res) } catch (e) {} })
            if (handlers.navigate) try { handlers.navigate(payload) } catch (e) {}
            return
          }
        }
      }
      // MAL / code flow: code & state in query (?code=...&state=...)
      if (value.indexOf('code=') !== -1) {
        var qp = parseQueryParams(value)
        if (qp.code) {
          var res2 = { code: qp.code, state: qp.state || '' }
          var resolvers2 = pendingAuthMAL.splice(0)
          if (resolvers2.length) {
            resolvers2.forEach(function (r) { try { r.resolve(res2) } catch (e) {} })
            if (handlers.navigate) try { handlers.navigate(payload) } catch (e) {}
            return
          }
          // Fallback: if no explicit MAL pending but AL pending, try to resolve reasonably.
          if (pendingAuthAL.length) {
            var resolvers3 = pendingAuthAL.splice(0)
            resolvers3.forEach(function (r) { try { r.resolve(res2) } catch (e) {} })
            if (handlers.navigate) try { handlers.navigate(payload) } catch (e) {}
            return
          }
        }
      }
    }
    if (handlers.navigate) try { handlers.navigate(payload) } catch (e) { console.error(e) }
  }

  function handleSubtitleEvent(payload) {
    payload = payload || {}
    var key = String(payload.hash || '') + ':' + String(payload.id)
    var list = subtitleHandlers[key]
    if (!list || !list.length) return
    var subtitle = payload.subtitle || payload
    var trackNumber = payload.trackNumber != null ? payload.trackNumber : payload.track_number
    for (var i = 0; i < list.length; i++) {
      try { list[i](subtitle, trackNumber) } catch (e) { console.error(e) }
    }
  }

  async function setupEvents() {
    try {
      if (tauri && tauri.event && tauri.event.listen) {
        await tauri.event.listen('native://navigate', function (e) { handleNavigate(e.payload) })
        await tauri.event.listen('native://error', function (e) { handlers.error && handlers.error(e.payload) })
        await tauri.event.listen('native://update-progress', function (e) {
          if (handlers.updateProgress) try { handlers.updateProgress(e.payload) } catch (err) {}
          if (handlers.downloadProgress) try { handlers.downloadProgress(e.payload) } catch (err) {}
        })
        await tauri.event.listen('native://download-progress', function (e) {
          if (handlers.downloadProgress) try { handlers.downloadProgress(e.payload) } catch (err) {}
        })
        await tauri.event.listen('torrent://error', function (e) { handlers.error && handlers.error(e.payload) })
        await tauri.event.listen('torrent://subtitle', function (e) { handleSubtitleEvent(e.payload) })
      }
    } catch (e) {
      console.warn('Failed to set up Tauri event bridges', e)
    }
  }

  function isFn(v) { return typeof v === 'function' }

  var native = {
    isApp: true,
    version: function () { return invoke('version') },
    openURL: function (url) { return invoke('open_url', { url: url }) },
    minimise: function () { return invoke('window_minimise') },
    maximise: function () { return invoke('window_maximise') },
    fullscreen: function (enabled) { return invoke('window_fullscreen', { enabled: !!enabled }) },
    close: function () { return invoke('window_close') },
    focus: function () { return invoke('window_focus') },
    restart: function () { return invoke('app_restart') },
    quit: function () { return invoke('app_quit') },
    setZoom: function (scale) { return invoke('set_zoom', { scale: scale }) },
    setHideToTray: function (enabled) { return invoke('set_hide_to_tray', { enabled: !!enabled }) },

    selectPlayer: function () { return invoke('select_player') },
    selectDownload: function (kind) { return invoke('select_download', { kind: kind }) },
    spawnPlayer: function (url) { return invoke('spawn_player', { url: url }) },

    getSettings: function () { return invoke('get_settings') },
    updateSettings: function (settings) { return invoke('update_settings', { settings: settings }) },
    setDOH: function (dns) { return invoke('set_doh', { dns: dns || '' }) },
    setAngle: function (angle) { return invoke('set_angle', { angle: angle || '' }) },

    getLogs: function () { return invoke('get_logs') },
    getDeviceInfo: function () { return invoke('get_device_info') },
    defaultTransparency: function () { return false },
    accentColor: function () { return invoke('accent_color') },

    checkUpdate: function () { return invoke('check_update') },
    updateReady: function () { return invoke('update_ready') },
    updateAndRestart: function () { return invoke('update_and_restart') },

    navigate: function (cb) { handlers.navigate = cb; return Promise.resolve() },
    errors: function (cb) { handlers.error = cb; return Promise.resolve() },
    updateProgress: function (cbOrPercent) {
      if (isFn(cbOrPercent)) { handlers.updateProgress = cbOrPercent; return Promise.resolve() }
      if (typeof cbOrPercent === 'number') { return invoke('torrent_download_progress', { percent: cbOrPercent }) }
      return Promise.resolve()
    },
    downloadProgress: function (cbOrPercent) {
      if (isFn(cbOrPercent)) { handlers.downloadProgress = cbOrPercent; return Promise.resolve() }
      if (typeof cbOrPercent === 'number') { return invoke('torrent_download_progress', { percent: cbOrPercent }) }
      return Promise.resolve()
    },

    addTorrent: function (id, mediaID, episode, background) {
      return invoke('add_torrent', {
        id: id,
        // Tauri deserializes command arguments from camelCase keys by default.
        mediaId: (mediaID == null ? 0 : mediaID),
        episode: (episode == null ? 0 : episode),
        background: !!background
      })
    },
    activeTorrents: function () { return invoke('torrent_active') },
    torrentInfo: function (hash) {
      if (!hash) {
        return invoke('torrent_active').then(function (active) {
          return (active && active[0]) || {
            hash: '', name: '', progress: 0,
            size: { total: 0, downloaded: 0, uploaded: 0 },
            speed: { down: 0, up: 0 },
            time: { remaining: 0, elapsed: 0 },
            peers: { seeders: 0, leechers: 0, wires: 0 },
            pieces: { total: 0, size: 0 }
          }
        })
      }
      return invoke('torrent_info', { hash: hash })
    },
    fileInfo: function (hash) { return invoke('torrent_file_info', { hash: hash }) },
    peerInfo: function (hash) { return invoke('torrent_peer_info', { hash: hash }) },
    trackers: function (hash) { return invoke('torrent_trackers', { hash: hash }) },
    protocolStatus: function (hash) { return invoke('torrent_protocol_status', { hash: hash }) },
    library: function () { return invoke('torrent_library') },
    cachedTorrents: function () { return invoke('torrent_cached') },
    rescanTorrents: function (hashes) { return invoke('torrent_rescan', { hashes: hashes || [] }) },
    deleteTorrents: function (hashes) { return invoke('torrent_delete', { hashes: hashes || [] }) },
    removeBackgroundTorrents: function (hashes) { return invoke('torrent_remove_background', { hashes: hashes || [] }) },
    checkAvailableSpace: function (path) { return invoke('torrent_check_available_space', { path: path }) },
    checkIncomingConnections: function (port) { return invoke('torrent_check_incoming_connections', { port: port || 0 }) },
    createNZB: function (hash, url) { return invoke('torrent_create_nzb', { hash: hash, url: url }) },
    createHTTPWebSeed: function (hash, url, authorization, fileIndex, rateLimit) {
      return invoke('torrent_create_http_webseed', {
        hash: hash, url: url, authorization: authorization,
        fileIndex: fileIndex, rateLimit: rateLimit
      })
    },

    attachments: function (hash, id) { return invoke('torrent_attachments', { hash: hash, id: id }) },
    tracks: function (hash, id) { return invoke('torrent_tracks', { hash: hash, id: id }) },
    subtitles: function (hash, id, cb) {
      var key = String(hash || '') + ':' + String(id)
      if (isFn(cb)) {
        if (!subtitleHandlers[key]) subtitleHandlers[key] = []
        subtitleHandlers[key].push(cb)
      }
      return invoke('torrent_subtitles', { hash: hash, id: id })
    },
    chapters: function (hash, id) { return invoke('torrent_chapters', { hash: hash, id: id }) },
    updatePeerCounts: function (hashes) { return invoke('torrent_update_peer_counts', { hashes: hashes || [] }) },

    openUIDevtools: function () { return invoke('open_ui_devtools') },
    openTorrentDevtools: function () { return invoke('open_torrent_devtools') },
    debug: function (levels) { return invoke('debug', { levels: levels || '' }) },
    profile: function (seconds) { return invoke('profile', { seconds: seconds || 0 }) },

    // --- Auth: best-effort, compatible with hayase://authorize deep-link flow ---
    authAL: function (url) {
      return new Promise(function (resolve, reject) {
        pendingAuthAL.push({ resolve: resolve, reject: reject })
        invoke('open_url', { url: url }).catch(function (e) {
          try { window.open(url, '_blank') } catch (_) {}
        })
        setTimeout(function () {
          var idx = pendingAuthAL.findIndex(function (p) { return p.resolve === resolve })
          if (idx >= 0) {
            pendingAuthAL.splice(idx, 1)
            reject(new Error('Authentication timed out or was cancelled'))
          }
        }, 300000)
      })
    },
    authMAL: function (url) {
      return new Promise(function (resolve, reject) {
        pendingAuthMAL.push({ resolve: resolve, reject: reject })
        invoke('open_url', { url: url }).catch(function (e) {
          try { window.open(url, '_blank') } catch (_) {}
        })
        setTimeout(function () {
          var idx = pendingAuthMAL.findIndex(function (p) { return p.resolve === resolve })
          if (idx >= 0) {
            pendingAuthMAL.splice(idx, 1)
            reject(new Error('Authentication timed out or was cancelled'))
          }
        }, 300000)
      })
    },

    // --- Casting / displays (not yet implemented) ---
    getDisplays: function (cb) { if (isFn(cb)) try { cb([]) } catch (e) {} ; return Promise.resolve([]) },
    castPlay: function () { return Promise.resolve() },
    castClose: function () { return Promise.resolve() },

    // --- Plugins / extensions (not yet implemented) ---
    pluginList: function () { return Promise.resolve([]) },
    pluginImport: function () { return Promise.reject(new Error('Plugins not implemented in Tauri backend yet')) },
    pluginDelete: function () { return Promise.resolve() },
    pluginPopup: function () { return Promise.resolve() },

    // --- OS/media integration (browser fallbacks / stubs) ---
    enableCORS: function () { return Promise.resolve() },
    setExperimentalGPU: function () { return Promise.resolve() },
    transparency: function () { return Promise.resolve() },
    toggleDiscordDetails: function () { return Promise.resolve() },
    unsafeUseInternalALAPI: function () { return Promise.resolve() },
    updateToNewEndpoint: function () { return Promise.resolve() },
    share: function (data) { return (navigator.share ? navigator.share(data) : Promise.resolve()) },

    setMediaSession: function (metadata, _mediaId, _duration) {
      try {
        if (navigator.mediaSession && metadata) {
          var artwork = []
          if (metadata.image) artwork.push({ src: metadata.image, sizes: '512x512', type: 'image/png' })
          navigator.mediaSession.metadata = new MediaMetadata({
            title: metadata.title || '',
            artist: metadata.description || '',
            artwork: artwork
          })
        }
      } catch (e) {}
      return Promise.resolve()
    },
    setPositionState: function (state, _paused) {
      try { if (navigator.mediaSession && state) navigator.mediaSession.setPositionState(state) } catch (e) {}
      return Promise.resolve()
    },
    setPlayBackState: function (state) {
      try { if (navigator.mediaSession) navigator.mediaSession.playbackState = state || 'none' } catch (e) {}
      return Promise.resolve()
    },
    setActionHandler: function (action, handler) {
      try { if (navigator.mediaSession) navigator.mediaSession.setActionHandler(action, handler || null) } catch (e) {}
      return Promise.resolve()
    }
  }

  window.native = Object.assign({}, window.native || {}, native)

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () { setupEvents() }, { once: true })
  } else {
    setupEvents()
  }
})();
