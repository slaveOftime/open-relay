/**
 * HTTP client facade.
 *
 * The implementation is split by concern (auth, sessions, SSE stream, attach
 * socket) so each piece can be read and tested on its own; this module is the
 * single import surface the app uses. Re-exports only, no behavior.
 */

export * from './client-http'
export * from './client-auth'
export * from './client-sessions'
export * from './client-nodes-push'
export * from './client-sse'
export * from './client-socket'
