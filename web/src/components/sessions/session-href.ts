/**
 * Links into a session detail page.
 *
 * `mode` picks the terminal or the log view, and `node` keeps a link pointed
 * at the same secondary node the row came from.
 */

export function buildSessionHref(sessionId: string, mode: 'attach' | 'logs', node?: string) {
  return `/session/${sessionId}?mode=${mode}${node ? `&node=${encodeURIComponent(node)}` : ''}`
}
