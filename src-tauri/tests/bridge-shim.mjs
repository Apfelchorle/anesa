import { readFileSync } from 'node:fs'
import vm from 'node:vm'
import assert from 'node:assert/strict'

// Exercises the fetch shim in `init-bridge.js` without a Tauri runtime. The
// Rust side of the proxy is covered by `cargo test --lib proxy::`.
const code = readFileSync(new URL('../init-bridge.js', import.meta.url), 'utf8')

const calls = []
let nativeFetchCalls = 0

const nativeFetch = async () => {
  nativeFetchCalls++
  return new Response('native', { status: 200, headers: { 'x-native': '1' } })
}

const invoke = async (cmd, args) => {
  calls.push({ cmd, args })
  if (cmd !== 'proxy_fetch') return null
  const body = Buffer.from(JSON.stringify({ ok: true, url: args.request.url })).toString('base64')
  return {
    status: 200,
    status_text: 'OK',
    headers: { 'content-type': 'application/json' },
    body_base64: body
  }
}

globalThis.window = {
  location: { href: 'http://tauri.localhost/' },
  fetch: nativeFetch,
  __TAURI_INTERNALS__: { invoke }
}
globalThis.document = {
  readyState: 'complete',
  addEventListener () {},
  createElement () { return {} }
}

vm.runInThisContext(code, { filename: 'init-bridge.js' })

assert.equal(typeof window.fetch, 'function')
assert.equal(typeof window.native.addTorrent, 'function')

// GET requests to allowlisted hosts are proxied.
let res = await window.fetch('https://hayase.ani.zip/v1/episodes?anilist_id=21')
assert.equal(res.status, 200)
assert.equal(res.url, 'https://hayase.ani.zip/v1/episodes?anilist_id=21')
assert.deepEqual(await res.json(), { ok: true, url: 'https://hayase.ani.zip/v1/episodes?anilist_id=21' })
assert.equal(calls.length, 1)
assert.equal(calls[0].cmd, 'proxy_fetch')
assert.equal(calls[0].args.request.method, 'GET')
assert.equal(calls[0].args.request.body_base64, null)

// Everything else keeps using native fetch.
res = await window.fetch('https://example.com/thing')
assert.equal(await res.text(), 'native')
assert.equal(nativeFetchCalls, 1)
assert.equal(calls.length, 1)

// POST bodies and headers cross the IPC boundary.
res = await window.fetch('https://graphql.anilist.co', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json', Origin: 'http://tauri.localhost' },
  body: JSON.stringify({ query: '{ Viewer { id } }' })
})
assert.equal(res.status, 200)
assert.equal(calls.length, 2)
const post = calls[1].args.request
assert.equal(post.method, 'POST')
assert.equal(post.headers['content-type'], 'application/json')
assert.equal(post.headers.origin, 'http://tauri.localhost')
assert.equal(Buffer.from(post.body_base64, 'base64').toString(), '{"query":"{ Viewer { id } }"}')

// Request objects preserve method/body.
const request = new Request('https://api.trace.moe/search?cutBorders', {
  method: 'POST',
  headers: { 'content-type': 'image/jpeg' },
  body: new Uint8Array([1, 2, 3, 4])
})
res = await window.fetch(request)
assert.equal(res.status, 200)
const reqCall = calls[2].args.request
assert.equal(reqCall.method, 'POST')
assert.deepEqual([...Buffer.from(reqCall.body_base64, 'base64')], [1, 2, 3, 4])

// Pre-aborted requests reject without invoking the backend.
const controller = new AbortController()
controller.abort()
await assert.rejects(
  window.fetch('https://api.aniskip.com/v2/skip-times/21/1', { signal: controller.signal }),
  (err) => err.name === 'AbortError'
)
assert.equal(calls.length, 3)

// Tauri argument casing is camelCase for multi-word args.
await window.native.addTorrent('abc', 21, 3, false)
const addCall = calls.find(c => c.cmd === 'add_torrent')
assert.deepEqual(addCall.args, { id: 'abc', mediaId: 21, episode: 3, background: false })

console.log('bridge shim tests passed')
