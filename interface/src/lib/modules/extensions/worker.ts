import { finalizer } from 'abslink'
import { expose } from 'abslink/w3c'

import type { NZBQuery, WebSeedQuery, WebSeedResult, WebSeedFile, AnimeQuery, TorrentResult, TorrentSource, NZBSource, SubtitleSource, Accuracy, WebSeedSource } from './types'

// First-party extension hosts allowed through the Tauri CORS proxy.
// Keep in sync with ALLOWED_PROXY_HOSTS in src-tauri/src/proxy.rs.
const PROXY_HOSTS: Record<string, true> = {
  'nekobt.to': true,
  'feed.animetosho.net': true,
  'amenzb.moe': true,
  'aninzb.moe': true,
  'api.tsukihime.org': true,
  'storage.tsukihime.org': true
}

const nativeFetch = typeof fetch === 'function' ? fetch.bind(globalThis) : undefined

export interface ProxyFetchRequest {
  url: string
  method: string
  headers: Record<string, string>
  body_base64: string | null
}

export interface ProxyFetchResult {
  status: number
  status_text: string
  headers: Record<string, string>
  body_base64: string
}

export type FetchRelay = (request: ProxyFetchRequest) => Promise<ProxyFetchResult>

// Installed by the main thread via `setFetchRelay`. Workers do not receive
// Tauri's initialization script, so they cannot call `invoke` themselves.
let fetchRelay: FetchRelay | null = null

function shouldProxyExtension (input: RequestInfo | URL): boolean {
  try {
    const raw = typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url
    const url = new URL(raw)
    return url.protocol === 'https:' && PROXY_HOSTS[url.hostname] === true
  } catch {
    return false
  }
}

function extBytesToBase64 (bytes: Uint8Array): string {
  let binary = ''
  const chunk = 0x8000
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk))
  }
  return btoa(binary)
}

function extBase64ToBytes (base64: string): Uint8Array {
  const binary = atob(base64 || '')
  const bytes = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i)
  return bytes
}

async function proxiedWorkerFetch (input: RequestInfo | URL, init?: RequestInit): Promise<Response> {
  if (!nativeFetch) throw new Error('fetch unavailable')
  if (!shouldProxyExtension(input)) return nativeFetch(input, init)
  if (!fetchRelay) return nativeFetch(input, init)

  const request = new Request(input instanceof Request ? input.clone() : input, init)
  const method = request.method.toUpperCase()
  const headers = new Headers(request.headers)

  let body_base64: string | null = null
  if (!['GET', 'HEAD'].includes(method)) {
    const body = new Uint8Array(await request.arrayBuffer())
    if (body.byteLength > 0) body_base64 = extBytesToBase64(body)
  }

  const headerObj: Record<string, string> = {}
  headers.forEach((value, key) => { headerObj[key] = value })

  const result = await fetchRelay({
    url: request.url,
    method,
    headers: headerObj,
    body_base64
  })

  const response = new Response(result.body_base64 ? (extBase64ToBytes(result.body_base64) as any) : null, {
    status: result.status,
    statusText: result.status_text,
    headers: result.headers || {}
  })
  Object.defineProperty(response, 'url', { value: request.url })
  return response
}

// In workers, prefer the Tauri proxy for allowlisted first-party extension hosts
// to avoid CORS failures (e.g. nekobt.to announcements/test). The main thread
// installs the relay; without it, fall back to native fetch.
const _fetch = proxiedWorkerFetch

// Also patch the worker-global fetch so extension code that calls bare `fetch()`
// (e.g. in test()) can reach allowlisted hosts without CORS issues.
if (typeof globalThis.fetch === 'function') {
  globalThis.fetch = ((input: RequestInfo | URL, init?: RequestInit) => _fetch(input, init)) as typeof fetch
}

interface AccuracyRule {
  setTo: Accuracy
  maxAllowed: Accuracy
}
type TSource = { url: string } & (TorrentSource | NZBSource | SubtitleSource | WebSeedSource)

const ACCURACIES = ['high', 'medium', 'low'] as const
const ACCURACY_RANK: Record<Accuracy, number> = { high: 0, medium: 1, low: 2 }

const RESULT_TYPES = ['batch', 'best', 'alt', undefined] as const

const QUERY_RULES: Record<string, AccuracyRule> = {
  anilistId: { setTo: 'medium', maxAllowed: 'high' }, // for people cheating using AL API to get media
  anidbAid: { setTo: 'medium', maxAllowed: 'high' },
  anidbEid: { setTo: 'high', maxAllowed: 'high' }, // usually means super duper accurate since manual mapping
  tvdbId: { setTo: 'medium', maxAllowed: 'high' },
  tvdbEId: { setTo: 'high', maxAllowed: 'high' }, // usually means super duper accurate since manual mapping
  imdbId: { setTo: 'medium', maxAllowed: 'high' },
  tmdbId: { setTo: 'medium', maxAllowed: 'high' },
  episode: { setTo: 'low', maxAllowed: 'high' }, // allow high since it might be a direct AL API mapping
  episodeCount: { setTo: 'low', maxAllowed: 'high' }, // this is for verifying if something is a batch
  absoluteEpisodeNumber: { setTo: 'low', maxAllowed: 'medium' }, // likely means some sketchy stuff is happening
  resolution: { setTo: 'low', maxAllowed: 'high' },
  exclusions: { setTo: 'low', maxAllowed: 'high' },
  titles: { setTo: 'low', maxAllowed: 'medium' }, // string matching, NO!
  media: { setTo: 'low', maxAllowed: 'medium' } // yeah, this should be a red flag
}

function queryProxy<T extends object> (query: T): { proxy: T, accessed: Set<string> } {
  const accessed = new Set<string>()
  const proxy = new Proxy(query, {
    get (target, prop, receiver) {
      if (typeof prop === 'string') accessed.add(prop)
      return Reflect.get(target, prop, receiver)
    }
  })
  return { proxy, accessed }
}

function computeAccuracy (accessed: Set<string>): Accuracy {
  let bestSetToRank = Infinity
  let worstMaxAllowedRank = 0

  for (const key of accessed) {
    const rule = QUERY_RULES[key]
    if (!rule) continue
    const setToRank = ACCURACY_RANK[rule.setTo]
    const maxAllowedRank = ACCURACY_RANK[rule.maxAllowed]
    if (setToRank < bestSetToRank) bestSetToRank = setToRank
    if (maxAllowedRank > worstMaxAllowedRank) worstMaxAllowedRank = maxAllowedRank
  }

  if (bestSetToRank === Infinity) bestSetToRank = 0

  return ACCURACIES[Math.max(bestSetToRank, worstMaxAllowedRank)]!
}

function sanitizeResults (results: TorrentResult[], max: Accuracy): TorrentResult[] {
  if (!Array.isArray(results)) return []
  const maxRank = ACCURACY_RANK[max]

  return results.reduce<TorrentResult[]>((acc, item) => {
    if (!item || typeof item !== 'object') return acc

    if (typeof item.hash !== 'string' || !/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/i.test(item.hash)) return acc
    if (typeof item.link !== 'string' || item.link.length === 0) return acc

    const accuracyRank = typeof item.accuracy === 'string' ? ACCURACY_RANK[item.accuracy] : undefined

    acc.push({
      hash: item.hash.toLowerCase(),
      accuracy: accuracyRank !== undefined && accuracyRank <= maxRank ? item.accuracy : max,
      title: typeof item.title === 'string' ? item.title : '',
      link: item.link,
      seeders: typeof item.seeders === 'number' && isFinite(item.seeders) ? Math.max(0, Math.round(item.seeders)) : 0,
      leechers: typeof item.leechers === 'number' && isFinite(item.leechers) ? Math.max(0, Math.round(item.leechers)) : 0,
      downloads: typeof item.downloads === 'number' && isFinite(item.downloads) ? Math.max(0, Math.round(item.downloads)) : 0,
      size: typeof item.size === 'number' && isFinite(item.size) ? Math.max(0, item.size) : 0,
      date: item.date instanceof Date && !isNaN(item.date.getTime()) ? item.date : new Date(0),
      id: typeof item.id === 'number' ? item.id : undefined,
      type: RESULT_TYPES.includes(item.type) ? item.type : undefined
    })
    return acc
  }, [])
}

type WorkerState =
  | { type: 'torrent', mod: TorrentSource & { url: string } }
  | { type: 'nzb', mod: NZBSource & { url: string } }
  | { type: 'subtitle', mod: SubtitleSource & { url: string } }
  | { type: 'http', mod: WebSeedSource & { url: string } }

export class ExtensionWorker {
  private _state: WorkerState | null = null
  mod!: Promise<{ url: string } & { test: () => Promise<boolean> }>

  setFetchRelay (relay: FetchRelay) {
    fetchRelay = relay
  }

  construct (code: string, type: 'torrent' | 'nzb' | 'subtitle' | 'http') {
    this.mod = this.load(code, type)
  }

  async load (code: string, type: 'torrent' | 'nzb' | 'subtitle' | 'http'): Promise<TSource> {
    // WARN: unsafe eval
    const url = URL.createObjectURL(new Blob([code], { type: 'application/javascript' }))
    const module = await import(/* @vite-ignore */url)
    URL.revokeObjectURL(url)
    const mod = module.default as TSource

    // @ts-expect-error w/e
    this._state = { type, mod }

    return mod
  }

  async loaded () {
    await this.mod
  }

  [finalizer] () {
    console.log('destroyed worker', self.name)
    self.close()
  }

  async url () {
    return (await this.mod).url
  }

  async single (
    query: AnimeQuery | NZBQuery<{ file: string }> | WebSeedQuery<{ file: WebSeedFile }> | Omit<AnimeQuery, 'resolution' | 'exclusions'>,
    options?: Record<string, string | number | boolean>
  ): Promise<TorrentResult[] | string | undefined | WebSeedResult | Array<{url: string, language: string}>> {
    const state = this._state!

    switch (state.type) {
      case 'torrent': {
        const { proxy, accessed } = queryProxy({ ...query as AnimeQuery, fetch: _fetch })
        return sanitizeResults(await state.mod.single(proxy, options), computeAccuracy(accessed))
      }
      case 'nzb': {
        return await state.mod.single({ ...query as NZBQuery<{ file: string }>, fetch: _fetch }, options)
      }
      case 'http': {
        const q = query as WebSeedQuery<{ file: WebSeedFile }>
        const res = await state.mod.single({ ...q, fetch: _fetch }, options)
        if (res) res.index ??= q.file.index
        return res
      }
      case 'subtitle': {
        return await state.mod.single({ ...query as Omit<AnimeQuery, 'resolution' | 'exclusions'>, fetch: _fetch }, options)
      }
    }
  }

  async batch (
    query: AnimeQuery | NZBQuery<{ files: string[], name: string }> | WebSeedQuery<{ files: WebSeedFile[], name: string }>,
    options?: Record<string, string | number | boolean>
  ): Promise<TorrentResult[] | string | undefined | WebSeedResult[]> {
    const state = this._state!

    switch (state.type) {
      case 'torrent': {
        const { proxy, accessed } = queryProxy({ ...query as AnimeQuery, fetch: _fetch })
        return sanitizeResults(await state.mod.batch(proxy, options), computeAccuracy(accessed))
      }
      case 'nzb': {
        return await state.mod.batch({ ...query as NZBQuery<{ files: string[], name: string }>, fetch: _fetch }, options)
      }
      case 'http': {
        return await state.mod.batch({ ...query as WebSeedQuery<{ files: WebSeedFile[], name: string }>, fetch: _fetch }, options)
      }
      case 'subtitle':
        throw new Error('batch not supported for subtitle sources')
    }
  }

  async movie (query: AnimeQuery, options?: Record<string, string | number | boolean>): Promise<TorrentResult[]> {
    const state = this._state!

    if (state.type !== 'torrent') return []
    const { proxy, accessed } = queryProxy({ ...query, fetch: _fetch })
    return sanitizeResults(await state.mod.movie(proxy, options), computeAccuracy(accessed))
  }

  async test () {
    return await (await this.mod).test()
  }
}

export default expose(new ExtensionWorker())
