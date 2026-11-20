import { writable, readable } from 'simple-store-svelte'

export const activityState = readable<'active' | 'inactive'>(document.hasFocus() ? 'active' : 'inactive', set => {
  set(document.hasFocus() ? 'active' : 'inactive')
  const ctrl = new AbortController()

  window.addEventListener('pointermove', () => set('active'), ctrl)
  window.addEventListener('focus', () => set('active'), ctrl)
  window.addEventListener('blur', () => set('inactive'), ctrl)

  document.addEventListener('mouseenter', () => set('active'), ctrl)
  document.addEventListener('mouseleave', () => {
    if (!document.hasFocus()) set('inactive')
  }, ctrl)
  return () => ctrl.abort()
})

// eslint-disable-next-line @typescript-eslint/no-explicit-any
let idleDetector: any = { userState: 'active', screenState: 'unlocked' }
try {
  // @ts-expect-error non-standard API
  const IdleDetectorCtor = typeof IdleDetector !== 'undefined' ? IdleDetector : undefined
  if (IdleDetectorCtor) {
    const detector = new IdleDetectorCtor()
    idleDetector = detector
    // Permission may be denied (common in WebView2/Tauri on Windows). Treat as non-fatal.
    const start = async () => {
      try {
        // @ts-expect-error non-standard API
        if (typeof IdleDetectorCtor.requestPermission === 'function') {
          // @ts-expect-error non-standard API
          const perm = await IdleDetectorCtor.requestPermission()
          if (perm === 'denied') return
        }
      } catch {
        // requestPermission may require user activation or be unsupported; try start anyway.
      }
      try {
        await detector.start({ threshold: 60_000 })
      } catch (error) {
        console.warn('Idle detection unavailable:', error)
        idleDetector = { userState: 'active', screenState: 'unlocked' }
      }
    }
    void start()
  }
} catch (error) {
  console.warn('Idle detection unavailable:', error)
  idleDetector = { userState: 'active', screenState: 'unlocked' }
}

export const idleState = readable<'active' | 'idle'>(idleDetector.userState, set => {
  if (!idleDetector) return set('active')
  set(idleDetector.userState)
  const ctrl = new AbortController()

  idleDetector.addEventListener('change', () => set(idleDetector.userState), ctrl)
  window.addEventListener('pointermove', () => set('active'), ctrl)

  return () => ctrl.abort()
})

export const lockedState = readable<'locked' | 'unlocked'>(idleDetector.screenState, set => {
  if (!idleDetector) return set('unlocked')
  set(idleDetector.screenState)
  const ctrl = new AbortController()

  idleDetector.addEventListener('change', () => set(idleDetector.screenState), ctrl)

  return () => ctrl.abort()
})

export const isPlaying = writable(false)
