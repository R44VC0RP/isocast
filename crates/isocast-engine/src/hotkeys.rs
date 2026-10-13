//! Platform boundary for global key registration. Implementations deliver
//! edges, never actions, and must not block the OS event thread.

use isocast_protocol::hotkeys::{Chord, Phase, Registration};
use std::sync::Arc;

#[derive(Clone)]
pub struct Key {
    pub binding: String,
    pub chord: Chord,
}

#[derive(Debug)]
pub struct Edge {
    pub binding: String,
    pub generation: u64,
    pub phase: Phase,
}

pub type OnKey = Arc<dyn Fn(Edge) + Send + Sync>;

pub trait GlobalHotkeys: Send + Sync {
    /// Replace registrations; report unsupported keys or OS conflicts per
    /// binding. `generation` rejects events queued by an old profile.
    fn replace(&self, keys: Vec<Key>, generation: u64, on_key: OnKey) -> Vec<Registration>;
}
