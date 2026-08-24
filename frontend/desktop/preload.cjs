// NonoClaw Desktop — preload.
//
// The web UI talks to the backend purely over HTTP/WebSocket and needs no
// Node capabilities. This preload intentionally exposes nothing; it exists
// so contextIsolation is explicit and future desktop-only bridges have a
// sanctioned place to live.

// No-op by design. Keep it this way unless a desktop bridge is actually
// needed — every added binding widens the renderer attack surface.
