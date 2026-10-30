//! Kubernetes access and port-forwarding for the kprtfwd daemon.
//!
//! [`Engine::reconcile`] runs exactly the forwards it is given: one context
//! actor per kubeconfig context in use, one forward actor per forward.

mod cache;
mod context;
pub mod discovery;
pub mod errors;
mod events;
mod forward;
pub mod portforward;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kprtfwd_core::state::{ContextState, Observed};
use kprtfwd_core::{Forward, ForwardId};
use tracing::info;

use crate::context::Context;
pub use crate::events::Event;
use crate::events::EventLog;
use crate::forward::ForwardHandle;
pub use crate::forward::LastError;

#[derive(Default)]
pub struct Engine {
    contexts: HashMap<String, Arc<Context>>,
    forwards: HashMap<ForwardId, ForwardHandle>,
    /// Kept here, not in the actors, so they survive stopping and starting.
    context_events: HashMap<String, EventLog>,
    forward_events: HashMap<ForwardId, EventLog>,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs exactly `wanted`: starts what is missing, stops what is no longer
    /// wanted, and restarts forwards whose settings changed.
    pub async fn reconcile(&mut self, wanted: &[Forward]) {
        let wanted_by_id: HashMap<&ForwardId, &Forward> =
            wanted.iter().map(|f| (&f.id, f)).collect();
        self.forwards.retain(|id, handle| {
            let keep = wanted_by_id
                .get(id)
                .is_some_and(|f| same_settings(f, &handle.spec));
            if !keep {
                handle.stop();
                info!(%id, "forward stopped");
            }
            keep
        });

        let needed: HashSet<&str> = wanted.iter().map(|f| f.context.as_str()).collect();
        self.contexts.retain(|name, context| {
            let keep = needed.contains(name.as_str());
            if !keep {
                context.stop();
                info!(context = %name, "context idle");
            }
            keep
        });
        for name in needed {
            if !self.contexts.contains_key(name) {
                let events = self
                    .context_events
                    .entry(name.to_owned())
                    .or_default()
                    .clone();
                let context = Context::start(name, events).await;
                self.contexts.insert(name.to_owned(), context);
            }
        }
        for (name, context) in &self.contexts {
            let namespaces: HashSet<String> = wanted
                .iter()
                .filter(|f| f.context == *name)
                .map(|f| f.namespace.clone())
                .collect();
            context.watch_namespaces(&namespaces);
        }

        for forward in wanted {
            if self.forwards.contains_key(&forward.id) {
                continue;
            }
            let context = self.contexts[&forward.context].clone();
            info!(id = %forward.id, "forward started");
            self.forwards.insert(
                forward.id.clone(),
                ForwardHandle::start(
                    forward.clone(),
                    context,
                    self.forward_events
                        .entry(forward.id.clone())
                        .or_default()
                        .clone(),
                ),
            );
        }
    }

    /// Stops a context and its forwards and starts them again from scratch.
    pub async fn reconnect(&mut self, context: &str, wanted: &[Forward]) {
        if let Some(old) = self.contexts.remove(context) {
            old.stop();
            self.forwards
                .retain(|_, handle| handle.spec.context != context);
        }
        self.reconcile(wanted).await;
    }

    /// The state of a context in use; `None` if no enabled forward uses it.
    pub fn context_state(&self, name: &str) -> Option<ContextState> {
        self.contexts.get(name).map(|context| context.state())
    }

    /// What the engine observes for a running forward.
    pub fn observe(&self, id: &ForwardId) -> Option<Observed> {
        let handle = self.forwards.get(id)?;
        let context = self.contexts.get(&handle.spec.context)?;
        Some(handle.observe(context))
    }

    pub fn last_error(&self, id: &ForwardId) -> Option<LastError> {
        self.forwards.get(id)?.last_error()
    }

    /// The events of a forward and of its context, oldest first.
    pub fn events(&self, id: &ForwardId, context: &str) -> Vec<Event> {
        let mut events: Vec<Event> = self
            .forward_events
            .get(id)
            .into_iter()
            .chain(self.context_events.get(context))
            .flat_map(EventLog::entries)
            .collect();
        events.sort_by_key(|event| event.at);
        events
    }

    pub fn shutdown(&mut self) {
        for handle in self.forwards.values() {
            handle.stop();
        }
        self.forwards.clear();
        for context in self.contexts.values() {
            context.stop();
        }
        self.contexts.clear();
    }
}

/// Whether a running forward can keep running with `wanted`'s settings.
fn same_settings(wanted: &Forward, running: &Forward) -> bool {
    wanted.context == running.context
        && wanted.namespace == running.namespace
        && wanted.service == running.service
        && wanted.remote_port == running.remote_port
        && wanted.local_port == running.local_port
}
