//! Per-client event stream behind `GET /v1/events` (SPEC §11 #32): progress
//! steps while an order runs, with a short in-memory replay for reconnects.
//! In-memory only, like every other store (AGENTS.md #7).

use contract::{Event, EventKind};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// An event with its stream id.
#[derive(Debug, Clone)]
pub struct Published {
    pub id: u64,
    pub event: Arc<Event>,
}

pub struct EventHub {
    next: AtomicU64,
    max: usize,
    ttl: Duration,
    clients: Mutex<HashMap<String, Client>>,
}

struct Client {
    tx: broadcast::Sender<Published>,
    ring: VecDeque<(Instant, Published)>,
}

impl EventHub {
    pub fn new(max: usize, ttl: Duration) -> Self {
        Self {
            next: AtomicU64::new(1),
            max: max.max(1),
            ttl,
            clients: Mutex::new(HashMap::new()),
        }
    }

    pub fn publish(&self, client_id: &str, event: Event) -> u64 {
        let mut clients = self.clients.lock().expect("event hub lock");
        let client = clients
            .entry(client_id.to_owned())
            .or_insert_with(|| Client {
                tx: broadcast::channel(self.max).0,
                ring: VecDeque::new(),
            });
        let published = Published {
            id: self.next.fetch_add(1, Ordering::Relaxed),
            event: Arc::new(event),
        };
        client.ring.push_back((Instant::now(), published.clone()));
        self.prune(&mut client.ring);
        // No subscriber is fine: the ring keeps it for a reconnect.
        let _ = client.tx.send(published.clone());
        published.id
    }

    /// Events after `after` still in the ring, then the live receiver. Both
    /// are taken under one lock, so nothing falls between them.
    pub fn subscribe(
        &self,
        client_id: &str,
        after: Option<u64>,
    ) -> (Vec<Published>, broadcast::Receiver<Published>) {
        let mut clients = self.clients.lock().expect("event hub lock");
        let client = clients
            .entry(client_id.to_owned())
            .or_insert_with(|| Client {
                tx: broadcast::channel(self.max).0,
                ring: VecDeque::new(),
            });
        self.prune(&mut client.ring);
        let replay = match after {
            Some(after) => client
                .ring
                .iter()
                .filter(|(_, p)| p.id > after)
                .map(|(_, p)| p.clone())
                .collect(),
            None => Vec::new(),
        };
        (replay, client.tx.subscribe())
    }

    fn prune(&self, ring: &mut VecDeque<(Instant, Published)>) {
        while ring.len() > self.max {
            ring.pop_front();
        }
        while ring.front().is_some_and(|(at, _)| at.elapsed() > self.ttl) {
            ring.pop_front();
        }
    }
}

/// The order being run, for code that publishes without a parameter for it.
#[derive(Debug, Clone)]
pub struct OrderRef {
    pub order_id: String,
    pub client_id: String,
    pub session_id: String,
    pub client_msg_id: Option<String>,
}

struct Current {
    order: OrderRef,
    last_step: RefCell<Option<String>>,
}

tokio::task_local! {
    static CURRENT: Current;
}

/// Runs `f` as `order`; [`progress`] inside it publishes for that order.
/// One order runs at a time (AGENTS.md #5), on the queue worker's task.
pub async fn scope<F: std::future::Future>(order: OrderRef, f: F) -> F::Output {
    CURRENT
        .scope(
            Current {
                order,
                last_step: RefCell::new(None),
            },
            f,
        )
        .await
}

/// Publishes a harness-worded step for the current order; repeats of the
/// last step and calls outside an order are dropped.
pub fn progress(hub: &EventHub, step: &str) {
    let _ = CURRENT.try_with(|cur| {
        if cur.last_step.borrow().as_deref() == Some(step) {
            return;
        }
        *cur.last_step.borrow_mut() = Some(step.to_owned());
        publish(
            hub,
            &cur.order,
            EventKind::Progress {
                step: step.to_owned(),
            },
        );
    });
}

pub fn publish(hub: &EventHub, order: &OrderRef, kind: EventKind) -> u64 {
    hub.publish(
        &order.client_id,
        Event {
            order_id: order.order_id.clone(),
            session_id: order.session_id.clone(),
            client_msg_id: order.client_msg_id.clone(),
            kind,
        },
    )
}

/// `ord_` + 128 bits of CSPRNG hex.
pub fn new_order_id() -> String {
    format!("ord_{}", crate::stores::pending::mint_token())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(client: &str) -> OrderRef {
        OrderRef {
            order_id: "ord_1".into(),
            client_id: client.into(),
            session_id: "s1".into(),
            client_msg_id: None,
        }
    }

    fn steps(events: &[Published]) -> Vec<String> {
        events
            .iter()
            .map(|p| match &p.event.kind {
                EventKind::Progress { step } => step.clone(),
                other => other.name().to_owned(),
            })
            .collect()
    }

    #[test]
    fn replay_starts_after_the_given_id_and_clients_are_isolated() {
        let hub = EventHub::new(8, Duration::from_secs(60));
        let first = publish(&hub, &order("a"), EventKind::Done);
        publish(&hub, &order("b"), EventKind::Done);
        publish(&hub, &order("a"), EventKind::Progress { step: "x".into() });
        let (replay, _) = hub.subscribe("a", Some(first));
        assert_eq!(steps(&replay), ["x"]);
        let (all, _) = hub.subscribe("a", Some(0));
        assert_eq!(all.len(), 2);
        let (none, _) = hub.subscribe("a", None);
        assert!(none.is_empty());
    }

    #[test]
    fn the_ring_is_capped_and_expires() {
        let hub = EventHub::new(2, Duration::from_secs(60));
        for _ in 0..5 {
            publish(&hub, &order("a"), EventKind::Done);
        }
        assert_eq!(hub.subscribe("a", Some(0)).0.len(), 2);
        let hub = EventHub::new(8, Duration::ZERO);
        publish(&hub, &order("a"), EventKind::Done);
        std::thread::sleep(Duration::from_millis(2));
        assert!(hub.subscribe("a", Some(0)).0.is_empty());
    }

    #[tokio::test]
    async fn a_subscriber_gets_live_events_without_duplicates() {
        let hub = EventHub::new(8, Duration::from_secs(60));
        let before = publish(&hub, &order("a"), EventKind::Done);
        let (replay, mut rx) = hub.subscribe("a", Some(before));
        assert!(replay.is_empty());
        let next = publish(&hub, &order("a"), EventKind::Done);
        assert_eq!(rx.recv().await.unwrap().id, next);
    }

    #[tokio::test]
    async fn progress_runs_only_inside_an_order_and_skips_repeats() {
        let hub = EventHub::new(8, Duration::from_secs(60));
        progress(&hub, "outside");
        scope(order("a"), async {
            progress(&hub, "Thinking…");
            progress(&hub, "Thinking…");
            progress(&hub, "Reading the image…");
            progress(&hub, "Thinking…");
        })
        .await;
        let (all, _) = hub.subscribe("a", Some(0));
        assert_eq!(
            steps(&all),
            ["Thinking…", "Reading the image…", "Thinking…"]
        );
    }
}
