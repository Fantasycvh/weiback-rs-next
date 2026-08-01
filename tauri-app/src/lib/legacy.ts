export const LEGACY_DISMISS_KEY = 'weiback.next.legacyDismissed'

export const hasDismissedLegacyPrompt = () =>
  localStorage.getItem(LEGACY_DISMISS_KEY) === 'true'

export const dismissLegacyPrompt = () => localStorage.setItem(LEGACY_DISMISS_KEY, 'true')
