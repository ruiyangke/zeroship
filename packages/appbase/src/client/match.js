// --- Route Matching (pure JS, no JSX) ---
export function matchRoute(pattern, pathname) {
  const patternParts = pattern.split('/').filter(Boolean)
  const pathParts = pathname.split('/').filter(Boolean)

  // Both empty = root
  if (patternParts.length === 0 && pathParts.length === 0) return {}

  // Catch-all
  const starIdx = patternParts.indexOf('*')
  if (starIdx !== -1) {
    const prefix = patternParts.slice(0, starIdx)
    if (pathParts.length < prefix.length) return null
    const params = {}
    for (let i = 0; i < prefix.length; i++) {
      if (prefix[i].startsWith(':')) {
        params[prefix[i].slice(1)] = pathParts[i]
      } else if (prefix[i] !== pathParts[i]) {
        return null
      }
    }
    params.slug = pathParts.slice(starIdx)
    return params
  }

  // Exact length match required
  if (patternParts.length !== pathParts.length) return null

  const params = {}
  for (let i = 0; i < patternParts.length; i++) {
    if (patternParts[i].startsWith(':')) {
      params[patternParts[i].slice(1)] = pathParts[i]
    } else if (patternParts[i] !== pathParts[i]) {
      return null
    }
  }
  return params
}
