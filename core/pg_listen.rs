use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::{Connection, Result};

/// A PostgreSQL NOTIFY event delivered to a listening session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgNotification {
    pub channel: String,
    pub payload: String,
    /// Backend PID of the session that executed NOTIFY (wire protocol field).
    pub pid: i32,
}

pub type PgNotifyDelivery = Arc<dyn Fn(PgNotification) + Send + Sync>;

struct SubscriberState {
    pid: i32,
    channels: HashSet<String>,
    inbox: VecDeque<PgNotification>,
    delivery: Option<PgNotifyDelivery>,
}

struct PgNotifyHubInner {
    next_id: u64,
    subscribers: HashMap<u64, SubscriberState>,
    channel_index: HashMap<String, HashSet<u64>>,
}

/// Database-scoped pub/sub hub for PostgreSQL LISTEN/NOTIFY.
pub struct PgNotifyHub {
    inner: Mutex<PgNotifyHubInner>,
    next_backend_pid: AtomicI32,
}

impl Default for PgNotifyHub {
    fn default() -> Self {
        Self::new()
    }
}

impl PgNotifyHub {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(PgNotifyHubInner {
                next_id: 1,
                subscribers: HashMap::new(),
                channel_index: HashMap::new(),
            }),
            next_backend_pid: AtomicI32::new(10_000),
        }
    }

    fn alloc_backend_pid(&self) -> i32 {
        self.next_backend_pid.fetch_add(1, Ordering::SeqCst)
    }

    pub fn register_subscriber(&self, pid: Option<i32>) -> u64 {
        let pid = pid.unwrap_or_else(|| self.alloc_backend_pid());
        let mut inner = self.inner.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.subscribers.insert(
            id,
            SubscriberState {
                pid,
                channels: HashSet::new(),
                inbox: VecDeque::new(),
                delivery: None,
            },
        );
        id
    }

    pub fn unregister_subscriber(&self, subscriber_id: u64) {
        let mut inner = self.inner.lock();
        let Some(state) = inner.subscribers.remove(&subscriber_id) else {
            return;
        };
        for channel in state.channels {
            if let Some(listeners) = inner.channel_index.get_mut(&channel) {
                listeners.remove(&subscriber_id);
                if listeners.is_empty() {
                    inner.channel_index.remove(&channel);
                }
            }
        }
    }

    pub fn set_delivery(&self, subscriber_id: u64, delivery: PgNotifyDelivery) {
        let mut inner = self.inner.lock();
        if let Some(state) = inner.subscribers.get_mut(&subscriber_id) {
            state.delivery = Some(delivery);
        }
    }

    pub fn listen(&self, subscriber_id: u64, channel: &str) {
        let mut inner = self.inner.lock();
        let Some(state) = inner.subscribers.get_mut(&subscriber_id) else {
            return;
        };
        if !state.channels.insert(channel.to_string()) {
            return;
        }
        inner
            .channel_index
            .entry(channel.to_string())
            .or_default()
            .insert(subscriber_id);
    }

    pub fn unlisten(&self, subscriber_id: u64, channel: Option<&str>) {
        let mut inner = self.inner.lock();
        let channels: Vec<String> = {
            let Some(state) = inner.subscribers.get(&subscriber_id) else {
                return;
            };
            match channel {
                Some(name) => vec![name.to_string()],
                None => state.channels.iter().cloned().collect(),
            }
        };
        for ch in &channels {
            if let Some(listeners) = inner.channel_index.get_mut(ch) {
                listeners.remove(&subscriber_id);
                if listeners.is_empty() {
                    inner.channel_index.remove(ch);
                }
            }
        }
        if let Some(state) = inner.subscribers.get_mut(&subscriber_id) {
            for ch in channels {
                state.channels.remove(&ch);
            }
        }
    }

    pub fn notify(&self, notifier_id: u64, channel: &str, payload: &str) {
        let deliveries: Vec<(u64, PgNotification, Option<PgNotifyDelivery>)> = {
            let inner = self.inner.lock();
            let notifier_pid = inner
                .subscribers
                .get(&notifier_id)
                .map(|s| s.pid)
                .unwrap_or(0);
            let notification = PgNotification {
                channel: channel.to_string(),
                payload: payload.to_string(),
                pid: notifier_pid,
            };
            let Some(listener_ids) = inner.channel_index.get(channel) else {
                return;
            };
            listener_ids
                .iter()
                .map(|&sub_id| {
                    let delivery = inner
                        .subscribers
                        .get(&sub_id)
                        .and_then(|s| s.delivery.clone());
                    (sub_id, notification.clone(), delivery)
                })
                .collect()
        };

        let mut inner = self.inner.lock();
        for (sub_id, notification, delivery) in deliveries {
            if let Some(state) = inner.subscribers.get_mut(&sub_id) {
                state.inbox.push_back(notification.clone());
                if let Some(deliver) = delivery {
                    deliver(notification);
                }
            }
        }
    }

    pub fn drain_inbox(&self, subscriber_id: u64) -> Vec<PgNotification> {
        let mut inner = self.inner.lock();
        let Some(state) = inner.subscribers.get_mut(&subscriber_id) else {
            return Vec::new();
        };
        state.inbox.drain(..).collect()
    }
}

/// Channels registered via LISTEN for this connection.
#[derive(Debug, Clone, Default)]
pub struct PgListenRegistry {
    channels: HashSet<String>,
}

impl PgListenRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn listen(&mut self, channel: &str) {
        self.channels.insert(channel.to_string());
    }

    pub fn unlisten(&mut self, channel: &str) {
        self.channels.remove(channel);
    }

    pub fn unlisten_all(&mut self) {
        self.channels.clear();
    }
}

fn ensure_subscriber(conn: &Connection) -> u64 {
    let mut slot = conn.pg_notify_subscriber_id.write();
    if let Some(id) = *slot {
        return id;
    }
    let id = conn.db.pg_notify_hub.register_subscriber(None);
    *slot = Some(id);
    id
}

pub fn handle_listen(conn: &Connection, channel: &str) -> Result<()> {
    let sub_id = ensure_subscriber(conn);
    conn.db.pg_notify_hub.listen(sub_id, channel);
    conn.pg_listen.write().listen(channel);
    Ok(())
}

pub fn handle_unlisten(conn: &Connection, channel: Option<&str>) -> Result<()> {
    let sub_id = ensure_subscriber(conn);
    conn.db.pg_notify_hub.unlisten(sub_id, channel);
    let mut registry = conn.pg_listen.write();
    match channel {
        Some(name) => registry.unlisten(name),
        None => registry.unlisten_all(),
    }
    Ok(())
}

pub fn handle_notify(conn: &Connection, channel: &str, payload: Option<&str>) -> Result<()> {
    let sub_id = ensure_subscriber(conn);
    conn.db
        .pg_notify_hub
        .notify(sub_id, channel, payload.unwrap_or(""));
    Ok(())
}

pub fn unregister_connection(conn: &Connection) {
    let sub_id = *conn.pg_notify_subscriber_id.read();
    if let Some(id) = sub_id {
        conn.db.pg_notify_hub.unregister_subscriber(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_delivers_to_other_subscriber() {
        let hub = PgNotifyHub::new();
        let a = hub.register_subscriber(None);
        let b = hub.register_subscriber(None);
        hub.listen(a, "alerts");
        hub.notify(b, "alerts", "hello");
        let received = hub.drain_inbox(a);
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].channel, "alerts");
        assert_eq!(received[0].payload, "hello");
        assert!(hub.drain_inbox(b).is_empty());
    }

    #[test]
    fn delivery_callback_invoked() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hub = PgNotifyHub::new();
        let listener = hub.register_subscriber(None);
        let notifier = hub.register_subscriber(None);
        hub.listen(listener, "alerts");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_cb = hits.clone();
        hub.set_delivery(
            listener,
            Arc::new(move |_| {
                hits_cb.fetch_add(1, Ordering::SeqCst);
            }),
        );
        hub.notify(notifier, "alerts", "hello");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn notify_delivers_to_self_when_listening() {
        let hub = PgNotifyHub::new();
        let a = hub.register_subscriber(None);
        hub.listen(a, "ch");
        hub.notify(a, "ch", "ping");
        let received = hub.drain_inbox(a);
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].payload, "ping");
    }
}
