use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc, RwLock,
};
use std::collections::HashSet;

/// One bit per slot 0–9. Bit N set = slot N occupied.
#[derive(Clone, Default)]
pub struct SlotRegistry(pub Arc<AtomicU16>);

impl SlotRegistry {
    pub fn mark(&self, slot: u8) {
        self.0.fetch_or(1 << slot, Ordering::AcqRel);
    }

    pub fn unmark(&self, slot: u8) {
        self.0.fetch_and(!(1 << slot), Ordering::AcqRel);
    }

    /// Acquire the first free slot (0–9). Returns None if all occupied.
    pub fn acquire(&self) -> Option<u8> {
        let bits = &self.0;
        loop {
            let current = bits.load(Ordering::Acquire);
            let slot = (0u8..10).find(|&i| current & (1 << i) == 0)?;
            let next = current | (1 << slot);
            if bits
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(slot);
            }
        }
    }
}

/// Sent by the bridge when a game connects natively (HANDSHAKE received on pipe).
/// Discovery should release the session on this slot (if any).
/// The ack channel is used to signal back when the session is fully closed.
pub struct HandoffSignal {
    pub slot: u8,
    pub ack: tokio::sync::oneshot::Sender<()>,
}

/// Set of client_ids currently managed by Auto-Discovery.
pub type ActiveSessions = Arc<RwLock<HashSet<String>>>;
