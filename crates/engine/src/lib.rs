//! Kubernetes access and port-forwarding for the kprtfwd daemon.
//!
//! [`Engine::reconcile`] runs exactly the forwards it is given: one context
//! actor per kubeconfig context in use, one forward actor per forward.

mod auth;
mod cache;
mod context;
pub mod discovery;
pub mod errors;
mod events;
#[cfg(test)]
mod fake;
mod forward;
pub mod kubeconfig;
pub mod portforward;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kprtfwd_core::state::{ContextState, Observed};
use kprtfwd_core::{Forward, ForwardId};
use kube::config::Kubeconfig;
use tracing::info;

use crate::context::Context;
pub use crate::events::Event;
use crate::events::EventLog;
use crate::forward::ForwardHandle;
pub use crate::forward::{LastError, port_is_free};

#[derive(Default)]
pub struct Engine {
    contexts: HashMap<String, Arc<Context>>,
    forwards: HashMap<ForwardId, ForwardHandle>,
    /// Kept here, not in the actors, so they survive stopping and starting.
    context_events: HashMap<String, EventLog>,
    forward_events: HashMap<ForwardId, EventLog>,
    /// Where contexts get their settings; `None` for the kubeconfig files.
    /// Tests give their own, so they never read `~/.kube/config`.
    kubeconfig: Option<Kubeconfig>,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_kubeconfig(kubeconfig: Kubeconfig) -> Self {
        Self {
            kubeconfig: Some(kubeconfig),
            ..Self::default()
        }
    }

    /// Runs exactly `wanted`: starts what is missing, stops what is no longer
    /// wanted, and restarts forwards whose settings changed. A restarted
    /// forward starts once the old one has closed its listeners, so it finds
    /// its port free.
    pub async fn reconcile(&mut self, wanted: &[Forward]) {
        let wanted_by_id: HashMap<&ForwardId, &Forward> =
            wanted.iter().map(|f| (&f.id, f)).collect();
        let stopped: Vec<ForwardHandle> = self
            .forwards
            .extract_if(|id, handle| {
                !wanted_by_id
                    .get(id)
                    .is_some_and(|f| same_settings(f, &handle.spec))
            })
            .map(|(id, handle)| {
                info!(%id, "forward stopped");
                handle
            })
            .collect();
        close(stopped).await;

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
                self.start_context(name, None).await;
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

    /// Stops a context and its forwards and starts them again from scratch:
    /// with the settings in `kubeconfig` if given, as the kubeconfig watcher
    /// does with the read it just compared, else from the kubeconfig files.
    pub async fn reconnect(
        &mut self,
        context: &str,
        wanted: &[Forward],
        kubeconfig: Option<&Kubeconfig>,
    ) {
        if let Some(old) = self.contexts.remove(context) {
            old.stop();
            let stopped = self
                .forwards
                .extract_if(|_, handle| handle.spec.context == context)
                .map(|(_, handle)| handle)
                .collect();
            close(stopped).await;
        }
        if wanted.iter().any(|forward| forward.context == context) {
            self.start_context(context, kubeconfig).await;
        }
        self.reconcile(wanted).await;
    }

    /// Starts the actor of a context, with the settings in `kubeconfig`, else
    /// in the engine's own, else in the kubeconfig files.
    async fn start_context(&mut self, name: &str, kubeconfig: Option<&Kubeconfig>) {
        let events = self
            .context_events
            .entry(name.to_owned())
            .or_default()
            .clone();
        let kubeconfig = kubeconfig.or(self.kubeconfig.as_ref());
        let context = Context::start(name, kubeconfig, events).await;
        self.contexts.insert(name.to_owned(), context);
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

    /// Drops the events of a forward that was deleted, so a forward added
    /// later under its ID starts without them. Called once the forward is
    /// stopped; a running one would go on writing to the log it holds.
    pub fn forget(&mut self, id: &ForwardId) {
        self.forward_events.remove(id);
    }

    /// Moves a forward's events to the new ID an edit gave it, so they go
    /// on as for an edit that keeps the ID, and the old ID is left without
    /// any. Called before the next reconcile, which stops the forward under
    /// its old ID and starts it under the new one.
    pub fn rename(&mut self, from: &ForwardId, to: &ForwardId) {
        match self.forward_events.remove(from) {
            Some(events) => self.forward_events.insert(to.clone(), events),
            None => self.forward_events.remove(to),
        };
    }

    /// A forward's events, oldest first. They outlive stopping the forward,
    /// but not deleting it ([`Engine::forget`]).
    pub fn forward_events(&self, id: &ForwardId) -> Vec<Event> {
        self.forward_events
            .get(id)
            .map(EventLog::entries)
            .unwrap_or_default()
    }

    /// A context's events, oldest first.
    pub fn context_events(&self, name: &str) -> Vec<Event> {
        self.context_events
            .get(name)
            .map(EventLog::entries)
            .unwrap_or_default()
    }

    /// The contexts in use.
    pub fn context_names(&self) -> Vec<String> {
        self.contexts.keys().cloned().collect()
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

/// Stops forwards and waits until their listeners are closed.
async fn close(forwards: Vec<ForwardHandle>) {
    futures::future::join_all(forwards.into_iter().map(ForwardHandle::close)).await;
}

/// Whether a running forward can keep running with `wanted`'s settings.
fn same_settings(wanted: &Forward, running: &Forward) -> bool {
    wanted.context == running.context
        && wanted.namespace == running.namespace
        && wanted.service == running.service
        && wanted.remote_port == running.remote_port
        && wanted.local_port == running.local_port
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use kprtfwd_core::state::Bind;

    use super::*;
    use crate::fake::{closed_port, free_port};

    /// Two contexts whose API server cannot be reached. The port is not
    /// reserved, so a test that checks why takes [`kubeconfig_at`] with a
    /// [`closed_port`].
    fn kubeconfig() -> Kubeconfig {
        kubeconfig_at(free_port())
    }

    /// Two contexts whose API server is on `port`.
    fn kubeconfig_at(port: u16) -> Kubeconfig {
        Kubeconfig::from_yaml(&format!(
            r#"
apiVersion: v1
kind: Config
clusters:
- name: down
  cluster:
    server: http://127.0.0.1:{port}
users:
- name: anonymous
  user: {{}}
contexts:
- name: staging
  context:
    cluster: down
    user: anonymous
- name: production
  context:
    cluster: down
    user: anonymous
"#
        ))
        .unwrap()
    }

    fn forward(context: &str, namespace: &str, service: &str) -> Forward {
        Forward {
            id: ForwardId::new(format!("{context}.{namespace}.{service}-80")),
            context: context.into(),
            namespace: namespace.into(),
            service: service.into(),
            remote_port: 80,
            local_port: free_port(),
            enabled: true,
        }
    }

    /// The forward's "started" and "stopped" events.
    fn starts_and_stops(engine: &Engine, id: &ForwardId) -> Vec<String> {
        engine
            .forward_events(id)
            .into_iter()
            .map(|event| event.message)
            .filter(|message| message == "started" || message == "stopped")
            .collect()
    }

    fn watched(engine: &Engine, context: &str, namespace: &str) -> bool {
        engine.contexts[context].namespace(namespace).is_some()
    }

    fn sorted(mut names: Vec<String>) -> Vec<String> {
        names.sort();
        names
    }

    #[tokio::test]
    async fn reconcile_runs_exactly_the_wanted_forwards() {
        let mut engine = Engine::with_kubeconfig(kubeconfig());
        let api = forward("staging", "shop", "api");
        let web = forward("staging", "web", "frontend");
        let prod = forward("production", "shop", "api");
        engine
            .reconcile(&[api.clone(), web.clone(), prod.clone()])
            .await;

        assert_eq!(sorted(engine.context_names()), ["production", "staging"]);
        assert!(watched(&engine, "staging", "shop") && watched(&engine, "staging", "web"));
        assert!(watched(&engine, "production", "shop") && !watched(&engine, "production", "web"));
        for forward in [&api, &web, &prod] {
            assert!(engine.observe(&forward.id).is_some(), "{}", forward.id);
            assert_eq!(starts_and_stops(&engine, &forward.id), ["started"]);
        }

        // The same settings keep a forward running.
        engine
            .reconcile(&[api.clone(), web.clone(), prod.clone()])
            .await;
        assert_eq!(starts_and_stops(&engine, &api.id), ["started"]);

        // Changed settings restart it, and its events go on.
        let moved = Forward {
            local_port: free_port(),
            ..api.clone()
        };
        engine
            .reconcile(&[moved.clone(), web.clone(), prod.clone()])
            .await;
        assert_eq!(
            starts_and_stops(&engine, &api.id),
            ["started", "stopped", "started"]
        );
        assert_eq!(engine.forwards[&api.id].spec, moved);
        assert_eq!(starts_and_stops(&engine, &web.id), ["started"]);

        // A namespace without forwards is no longer watched, and a context
        // without forwards stops; the events of both stay.
        engine.reconcile(std::slice::from_ref(&moved)).await;
        assert_eq!(engine.context_names(), ["staging"]);
        assert!(watched(&engine, "staging", "shop") && !watched(&engine, "staging", "web"));
        assert_eq!(engine.context_state("production"), None);
        assert!(engine.observe(&prod.id).is_none());
        assert_eq!(starts_and_stops(&engine, &prod.id), ["started", "stopped"]);
        assert_eq!(starts_and_stops(&engine, &web.id), ["started", "stopped"]);

        engine.reconcile(&[]).await;
        assert!(engine.context_names().is_empty());
        assert_eq!(
            starts_and_stops(&engine, &api.id),
            ["started", "stopped", "started", "stopped"]
        );
    }

    #[tokio::test]
    async fn a_forward_added_after_a_delete_starts_without_old_events() {
        let mut engine = Engine::with_kubeconfig(kubeconfig());
        let api = forward("staging", "shop", "api");
        engine.reconcile(std::slice::from_ref(&api)).await;
        // Deleted: no longer wanted, then forgotten.
        engine.reconcile(&[]).await;
        assert_eq!(starts_and_stops(&engine, &api.id), ["started", "stopped"]);
        engine.forget(&api.id);
        assert!(engine.forward_events(&api.id).is_empty());

        // A new forward with the same ID, as discovery gives it again.
        engine.reconcile(std::slice::from_ref(&api)).await;
        assert_eq!(starts_and_stops(&engine, &api.id), ["started"]);
        // Forgetting another forward leaves this one's events.
        engine.forget(&ForwardId::new("staging.shop.web-80"));
        assert_eq!(starts_and_stops(&engine, &api.id), ["started"]);
    }

    #[tokio::test]
    async fn the_events_of_a_forward_follow_its_new_id() {
        let mut engine = Engine::with_kubeconfig(kubeconfig());
        let api = forward("staging", "shop", "api");
        engine.reconcile(std::slice::from_ref(&api)).await;

        // An edit moved it to another namespace, which gave it a new ID.
        let moved = Forward {
            id: ForwardId::new("staging.web.api-80"),
            namespace: "web".into(),
            ..api.clone()
        };
        engine.rename(&api.id, &moved.id);
        engine.reconcile(std::slice::from_ref(&moved)).await;
        assert_eq!(
            starts_and_stops(&engine, &moved.id),
            ["started", "stopped", "started"]
        );
        assert!(engine.forward_events(&api.id).is_empty());
        assert!(engine.observe(&api.id).is_none());
        assert!(engine.observe(&moved.id).is_some());

        // A forward that never ran takes no events along, and none that
        // were left under its new ID stay.
        engine.reconcile(&[]).await;
        let web = ForwardId::new("staging.shop.web-80");
        engine.rename(&web, &moved.id);
        assert!(engine.forward_events(&moved.id).is_empty());
    }

    #[tokio::test]
    async fn context_events_outlive_the_context() {
        let mut engine = Engine::with_kubeconfig(kubeconfig_at(closed_port()));
        let api = forward("staging", "shop", "api");
        engine.reconcile(std::slice::from_ref(&api)).await;
        let started = Instant::now();
        while engine.context_events("staging").is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "no context event"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let events = engine.context_events("staging");
        assert_eq!(events[0].message, "API server unreachable (refused)");

        engine.reconcile(&[]).await;
        assert_eq!(engine.context_events("staging"), events);
        engine.reconcile(std::slice::from_ref(&api)).await;
        assert_eq!(engine.context_events("staging")[0], events[0]);
    }

    #[tokio::test]
    async fn a_context_missing_from_the_kubeconfig_is_a_config_error() {
        let mut engine = Engine::with_kubeconfig(kubeconfig());
        let api = forward("gone", "shop", "api");
        engine.reconcile(std::slice::from_ref(&api)).await;
        let Some(ContextState::ConfigError { message }) = engine.context_state("gone") else {
            panic!("{:?}", engine.context_state("gone"));
        };
        assert!(message.contains("gone"), "{message}");
        let events = engine.context_events("gone");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, format!("kubeconfig: {message}"));
        assert!(matches!(
            engine.observe(&api.id).map(|observed| observed.context),
            Some(ContextState::ConfigError { .. })
        ));
    }

    #[tokio::test]
    async fn reconnect_starts_one_context_and_its_forwards_afresh() {
        let mut engine = Engine::with_kubeconfig(kubeconfig());
        let api = forward("staging", "shop", "api");
        let prod = forward("production", "shop", "api");
        let wanted = [api.clone(), prod.clone()];
        engine.reconcile(&wanted).await;
        let staging = engine.contexts["staging"].clone();
        let production = engine.contexts["production"].clone();

        engine.reconnect("staging", &wanted, None).await;
        assert!(!Arc::ptr_eq(&staging, &engine.contexts["staging"]));
        assert!(Arc::ptr_eq(&production, &engine.contexts["production"]));
        assert!(watched(&engine, "staging", "shop"));
        assert_eq!(
            starts_and_stops(&engine, &api.id),
            ["started", "stopped", "started"]
        );
        assert_eq!(starts_and_stops(&engine, &prod.id), ["started"]);

        // A context not in use is just reconciled.
        engine.reconnect("idle", &wanted, None).await;
        assert_eq!(sorted(engine.context_names()), ["production", "staging"]);
        assert_eq!(starts_and_stops(&engine, &prod.id), ["started"]);
    }

    #[tokio::test]
    async fn reconnect_uses_the_kubeconfig_it_is_given() {
        let mut engine = Engine::with_kubeconfig(kubeconfig());
        let api = forward("staging", "shop", "api");
        let wanted = [api.clone()];
        engine.reconcile(&wanted).await;
        assert!(!matches!(
            engine.context_state("staging"),
            Some(ContextState::ConfigError { .. })
        ));

        // A read without the context, as the watcher would pass it.
        let without: Kubeconfig = Kubeconfig {
            contexts: vec![],
            ..kubeconfig()
        };
        engine.reconnect("staging", &wanted, Some(&without)).await;
        let Some(ContextState::ConfigError { message }) = engine.context_state("staging") else {
            panic!("{:?}", engine.context_state("staging"));
        };
        assert!(message.contains("staging"), "{message}");
        assert_eq!(
            starts_and_stops(&engine, &api.id),
            ["started", "stopped", "started"]
        );

        // Without one, the engine's own counts again.
        engine.reconnect("staging", &wanted, None).await;
        assert!(!matches!(
            engine.context_state("staging"),
            Some(ContextState::ConfigError { .. })
        ));
    }

    /// The outcome of the first attempt to bind the port of the forward
    /// running as `id` now.
    async fn first_bind(engine: &Engine, id: &ForwardId) -> Bind {
        let started = Instant::now();
        loop {
            let bind = engine.observe(id).unwrap().bind;
            if bind != Bind::Pending {
                return bind;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "no bind attempt"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A forward started again on its port binds it at the first attempt:
    /// the old forward's listeners are closed before. The engine runs in a
    /// worker task, as in the daemon, where tokio tends to run a task just
    /// spawned before the one aborted just before it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restarted_forward_binds_its_port_at_once() {
        tokio::spawn(async {
            let own = format!("(pid {})", std::process::id());
            for _ in 0..3 {
                // Not one of port 0, which another socket may take while
                // it is closed.
                let port = closed_port();
                match restarts_bind_at_once(port).await {
                    Ok(()) => return,
                    // A process another test started inherited the
                    // listener (see `fake::held_by_another_process`), and
                    // may have ended before lsof looked.
                    Err(Bind::Busy { holder: None }) => {}
                    Err(Bind::Busy {
                        holder: Some(holder),
                    }) if !holder.ends_with(&own) => {}
                    Err(bind) => panic!("port {port}: {bind:?}"),
                }
            }
            panic!("ports held by other processes in three attempts");
        })
        .await
        .unwrap();
    }

    /// Starts a forward on `port` and restarts it there ten times through
    /// an edit and ten times through a reconnect. Returns the first bind
    /// that is not `Bound`.
    async fn restarts_bind_at_once(port: u16) -> Result<(), Bind> {
        let bound = |bind: Bind| {
            if bind == Bind::Bound {
                Ok(())
            } else {
                Err(bind)
            }
        };
        let mut engine = Engine::with_kubeconfig(kubeconfig());
        let api = Forward {
            local_port: port,
            ..forward("staging", "shop", "api")
        };
        engine.reconcile(std::slice::from_ref(&api)).await;
        bound(first_bind(&engine, &api.id).await)?;

        let edited = Forward {
            namespace: "web".into(),
            ..api.clone()
        };
        for round in 0..10 {
            let wanted = if round % 2 == 0 { &edited } else { &api };
            engine.reconcile(std::slice::from_ref(wanted)).await;
            bound(first_bind(&engine, &api.id).await)?;
            engine
                .reconnect("staging", std::slice::from_ref(wanted), None)
                .await;
            bound(first_bind(&engine, &api.id).await)?;
        }
        let busy: Vec<String> = engine
            .forward_events(&api.id)
            .into_iter()
            .map(|event| event.message)
            .filter(|message| message.contains("busy"))
            .collect();
        assert!(busy.is_empty(), "{busy:?}");
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_stops_everything() {
        let mut engine = Engine::with_kubeconfig(kubeconfig());
        let api = forward("staging", "shop", "api");
        engine.reconcile(std::slice::from_ref(&api)).await;
        engine.shutdown();
        assert!(engine.context_names().is_empty());
        assert!(engine.observe(&api.id).is_none());
        assert_eq!(starts_and_stops(&engine, &api.id), ["started", "stopped"]);
    }

    #[test]
    fn only_the_forwards_own_settings_matter() {
        let api = forward("staging", "shop", "api");
        assert!(same_settings(&api, &api));
        assert!(same_settings(
            &Forward {
                enabled: false,
                ..api.clone()
            },
            &api
        ));
        let changes = [
            Forward {
                context: "production".into(),
                ..api.clone()
            },
            Forward {
                namespace: "web".into(),
                ..api.clone()
            },
            Forward {
                service: "frontend".into(),
                ..api.clone()
            },
            Forward {
                remote_port: 8080,
                ..api.clone()
            },
            Forward {
                local_port: api.local_port.wrapping_add(1),
                ..api.clone()
            },
        ];
        for changed in changes {
            assert!(!same_settings(&changed, &api), "{changed:?}");
        }
    }
}
