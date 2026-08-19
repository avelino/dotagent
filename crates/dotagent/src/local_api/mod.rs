//! Local API — a Unix-socket JSON-lines server for same-machine clients.
//!
//! dotagent is a harness: this socket is a transport for triggers (in) and
//! run output (out). No conversation state, no LLM, no agent logic — all of
//! that lives behind [`server::LocalApiHandler`], the seam the daemon's
//! gateway integration implements.
//!
//! Wire shape, one JSON object per line in both directions:
//!
//! ```text
//! -> {"id":"u1","method":"message.send","params":{"session_id":"default","text":"status?"}}
//! <- {"id":"u1","result":{"accepted":true}}
//! <- {"event":"typing","session_id":"default"}
//! <- {"event":"reply","session_id":"default","text":"..."}
//! ```
//!
//! The socket lives at `~/.config/dotagent/api.sock` with 0600 permissions.
//! Threat model (docs/security/threat-model.md, V8/V9): it grants
//! user-equivalent power, so every peer is identified by kernel credentials
//! (`SO_PEERCRED` on Linux, `SOL_LOCAL` on Darwin) and every log line can
//! name the pid behind a request.

pub mod protocol;
pub mod server;
