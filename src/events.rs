//! GET /api/events: control changes as Server-Sent Events.
//!
//! Phase 0: only the per-server state that AppState holds; the endpoint lands with G4 D3.

/// The open /api/events streams.
#[derive(Debug, Default)]
pub struct EventStreams {}
