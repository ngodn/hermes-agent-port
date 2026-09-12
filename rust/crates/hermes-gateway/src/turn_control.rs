//! Route-scoped controls for one admitted agent turn.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopOutcome {
    Idle,
    Requested,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SteerOutcome {
    Idle,
    Empty,
    Queued { preview: String },
}

#[derive(Debug, Eq, PartialEq)]
pub struct TurnCompletion {
    pub interrupted: bool,
    pub pending_steer: Option<String>,
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
    pending_steer: Arc<Mutex<Option<String>>>,
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

    fn queue_steer(&self, text: &str) {
        let mut pending = self.pending_steer.lock().unwrap();
        match pending.as_mut() {
            Some(existing) => {
                existing.push('\n');
                existing.push_str(text);
            }
            None => *pending = Some(text.to_owned()),
        }
    }

    pub(crate) fn take_pending_steer(&self) -> Option<String> {
        self.pending_steer.lock().unwrap().take()
    }

    pub(crate) fn restore_pending_steer(&self, text: &str) {
        let mut pending = self.pending_steer.lock().unwrap();
        match pending.as_mut() {
            // Match Python's restash ordering: guidance that arrived after the
            // drain stays first, followed by the older undelivered payload.
            Some(existing) => {
                existing.push('\n');
                existing.push_str(text);
            }
            None => *pending = Some(text.to_owned()),
        }
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
    pub fn finish(mut self) -> TurnCompletion {
        let mut state = self.registry.inner.lock().unwrap();
        let interrupted = self.control.is_cancelled();
        if state
            .active
            .get(&self.route_key)
            .is_some_and(|active| active.generation == self.generation)
        {
            state.active.remove(&self.route_key);
        }
        self.registered = false;
        let pending_steer = self.control.take_pending_steer();
        TurnCompletion {
            interrupted,
            pending_steer: if interrupted { None } else { pending_steer },
        }
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

    pub fn steer(&self, route_key: &str, text: &str) -> SteerOutcome {
        let state = self.inner.lock().unwrap();
        let Some(active) = state.active.get(route_key) else {
            return SteerOutcome::Idle;
        };
        if active.control.is_cancelled() {
            return SteerOutcome::Idle;
        }
        let cleaned = text.trim_matches(crate::python_value::python_whitespace);
        if cleaned.is_empty() {
            return SteerOutcome::Empty;
        }
        active.control.queue_steer(cleaned);
        let mut preview = cleaned.chars().take(60).collect::<String>();
        if cleaned.chars().count() > 60 {
            preview.push_str("...");
        }
        SteerOutcome::Queued { preview }
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
        assert!(!registration.finish().interrupted);
        assert_eq!(registry.stop("route"), StopOutcome::Idle);
    }

    #[test]
    fn steer_normalizes_accumulates_and_previews_in_arrival_order() {
        let registry = TurnControlRegistry::default();
        let registration = registry.register("route");

        assert_eq!(
            registry.steer("route", "  first note  "),
            SteerOutcome::Queued {
                preview: "first note".into(),
            }
        );
        let long = "界".repeat(61);
        assert_eq!(
            registry.steer("route", &long),
            SteerOutcome::Queued {
                preview: format!("{}...", "界".repeat(60)),
            }
        );
        let expected = format!("first note\n{long}");
        assert_eq!(registration.control().take_pending_steer(), Some(expected));
    }

    #[test]
    fn completion_returns_leftover_steer_but_hard_stop_discards_it() {
        let registry = TurnControlRegistry::default();
        let steered = registry.register("steered");
        assert!(matches!(
            registry.steer("steered", "next question"),
            SteerOutcome::Queued { .. }
        ));
        let completion = steered.finish();
        assert!(!completion.interrupted);
        assert_eq!(completion.pending_steer.as_deref(), Some("next question"));

        let stopped = registry.register("stopped");
        assert!(matches!(
            registry.steer("stopped", "must be discarded"),
            SteerOutcome::Queued { .. }
        ));
        assert_eq!(registry.stop("stopped"), StopOutcome::Requested);
        let completion = stopped.finish();
        assert!(completion.interrupted);
        assert_eq!(completion.pending_steer, None);
    }

    #[test]
    fn restash_keeps_guidance_that_arrived_during_the_drain() {
        let registry = TurnControlRegistry::default();
        let registration = registry.register("route");
        assert!(matches!(
            registry.steer("route", "older"),
            SteerOutcome::Queued { .. }
        ));
        let drained = registration.control().take_pending_steer().unwrap();
        assert!(matches!(
            registry.steer("route", "newer"),
            SteerOutcome::Queued { .. }
        ));
        registration.control().restore_pending_steer(&drained);
        assert_eq!(
            registration.control().take_pending_steer().as_deref(),
            Some("newer\nolder")
        );
    }
}
