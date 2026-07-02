//! Domain services layered over the Codex/Tsundoku/MangaBaka clients and the
//! database. Currently the Codex identity broker plus the per-user library
//! snapshot; the remaining recommendation services land with the rest of the
//! services phase.

pub mod codex_identity;
pub mod library_snapshot;
pub mod read_state;
pub mod tsundoku_cache;
