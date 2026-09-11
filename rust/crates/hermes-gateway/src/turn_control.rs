//! Route-scoped controls for one admitted agent turn.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopOutcome {
    Idle,
    Requested,
}

#[derive(Clone, Default)]
pub struct TurnControlRegistry {
    inner: Arc<Mutex<RegistryState>>,
}

#[derive(Default)]
struct RegistryState {
    next_generation: u64,
    active: HashMap<String, ActiveTurn>,
}

struct ActiveTurn {
    generation: u64,
    control: Arc<TurnControl>,
}

#[derive(Clone, Default)]
pub struct TurnControl {
    cancelled: CancellationToken,
}

impl TurnControl {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.cancelled.cancelled().await;
    }

    fn cancel(&self) {
        self.cancelled.cancel();
    }
}

pub struct TurnControlRegistration {
    registry: TurnControlRegistry,
    route_key: String,
    generation: u64,
    control: Arc<TurnControl>,
    registered: bool,
}

impl TurnControlRegistration {
    pub fn control(&self) -> &Arc<TurnControl> {
        &self.control
    }

    /// Atomically retire this generation and report whether stop won before
    /// completion. A later stop sees no active turn and cannot suppress an
    /// already-completed reply.
    pub fn finish(mut self) -> bool {
        let mut state = self.registry.inner.lock().unwrap();
        let cancelled = self.control.is_cancelled();
        if state
            .active
            .get(&self.route_key)
            .is_some_and(|active| active.generation == self.generation)
        {
            state.active.remove(&self.route_key);
        }
        self.registered = false;
        cancelled
    }
}

impl Drop for TurnControlRegistration {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        let mut state = self.registry.inner.lock().unwrap();
        if state
            .active
            .get(&self.route_key)
            .is_some_and(|active| active.generation == self.generation)
        {
            state.active.remove(&self.route_key);
        }
    }
}

impl TurnControlRegistry {
    pub fn register(&self, route_key: &str) -> TurnControlRegistration {
        let mut state = self.inner.lock().unwrap();
        state.next_generation = state.next_generation.wrapping_add(1);
        let generation = state.next_generation;
        let control = Arc::new(TurnControl::default());
        if let Some(replaced) = state.active.insert(
            route_key.to_owned(),
            ActiveTurn {
                generation,
                control: control.clone(),
            },
        ) {
            replaced.control.cancel();
        }
        TurnControlRegistration {
            registry: self.clone(),
            route_key: route_key.to_owned(),
            generation,
            control,
            registered: true,
        }
    }

    pub fn stop(&self, route_key: &str) -> StopOutcome {
        let state = self.inner.lock().unwrap();
        match state.active.get(route_key) {
            Some(active) => {
                active.control.cancel();
                StopOutcome::Requested
            }
            None => StopOutcome::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_wakes_the_active_generation_and_idle_stop_is_a_no_op() {
        let registry = TurnControlRegistry::default();
        assert_eq!(registry.stop("route"), StopOutcome::Idle);

        let registration = registry.register("route");
        let control = registration.control().clone();
        assert_eq!(registry.stop("route"), StopOutcome::Requested);
        control.cancelled().await;
        assert!(control.is_cancelled());

        drop(registration);
        assert_eq!(registry.stop("route"), StopOutcome::Idle);
    }

    #[test]
    fn stale_registration_cannot_remove_its_replacement() {
        let registry = TurnControlRegistry::default();
        let stale = registry.register("route");
        let current = registry.register("route");

        assert!(stale.control().is_cancelled());
        drop(stale);
        assert_eq!(registry.stop("route"), StopOutcome::Requested);
        assert!(current.control().is_cancelled());
    }

    #[test]
    fn route_keys_are_isolated() {
        let registry = TurnControlRegistry::default();
        let one = registry.register("one");
        let two = registry.register("two");

        assert_eq!(registry.stop("one"), StopOutcome::Requested);
        assert!(one.control().is_cancelled());
        assert!(!two.control().is_cancelled());
    }

    #[test]
    fn completion_linearizes_before_a_late_stop() {
        let registry = TurnControlRegistry::default();
        let registration = registry.register("route");
        assert!(!registration.finish());
        assert_eq!(registry.stop("route"), StopOutcome::Idle);
    }
}
