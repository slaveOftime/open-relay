/** Accept only local proxied-app deep links from the login route. */
export function proxyLoginDestination(href: string): string | null {
  const loginUrl = new URL(href)
  if (loginUrl.pathname !== '/login') return null

  const next = loginUrl.searchParams.get('next')
  if (!next?.startsWith('/apps/') || next.includes('\\')) return null
  try {
    const destination = new URL(next, loginUrl.origin)
    if (destination.origin !== loginUrl.origin || !destination.pathname.startsWith('/apps/')) {
      return null
    }
    return `${destination.pathname}${destination.search}${destination.hash}`
  } catch {
    return null
  }
}
