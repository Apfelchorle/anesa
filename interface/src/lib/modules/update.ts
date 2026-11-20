// App updates are managed by the distribution/build; disable in-app update checks.
export const outdatedComponent = Promise.resolve(undefined as 'ui' | 'client' | undefined)

export const uiUpdate = new Promise(resolve => {
  if ('serviceWorker' in navigator) navigator.serviceWorker.addEventListener('controllerchange', resolve, { once: true })
})
