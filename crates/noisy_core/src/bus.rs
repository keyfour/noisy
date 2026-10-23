//! The event bus: a `broadcast` channel of typed [`NoisyEvent`] values.
//!
//! Every module publishes here and every subscriber listens here
//! (specification §3.1). Capacity is configurable; lagged subscribers receive
//! a `noisy.bus_lagged` warning event and resume from the latest event.

use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::{self, error::RecvError};

/// Canonical fully-qualified names of the events the core itself emits
/// (`source = "noisy"`). See specification §3.6.
pub mod core_events {
    pub const STARTED: &str = "noisy.started";
    pub const READY: &str = "noisy.ready";
    pub const SERVICE_STARTED: &str = "noisy.service_started";
    pub const SERVICE_STOPPED: &str = "noisy.service_stopped";
    pub const SHUTDOWN: &str = "noisy.shutdown";
    pub const SHUTDOWN_TIMEOUT: &str = "noisy.shutdown_timeout";
    pub const BUS_LAGGED: &str = "noisy.bus_lagged";
    pub const SCRIPT_LOADED: &str = "noisy.script_loaded";
    pub const SCRIPT_RELOADED: &str = "noisy.script_reloaded";
    pub const SCRIPT_ERROR: &str = "noisy.script_error";
}

/// Minimum size accepted for a bus channel; a zero capacity is clamped up.
pub const MIN_BUS_CAPACITY: usize = 1;

/// Current wall-clock time as epoch milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The common envelope of every event on the bus (specification §3.5).
#[derive(Debug, Clone, PartialEq)]
pub struct NoisyEvent {
    /// Module that emitted this event, e.g. `"serial"`.
    pub source: String,
    /// Event kind within that module, e.g. `"data_received"`.
    pub kind: String,
    /// Fully qualified name: `"{source}.{kind}"`.
    pub name: String,
    /// Event payload (JSON).
    pub payload: Value,
    /// Epoch-millisecond timestamp.
    pub timestamp: u64,
}

impl NoisyEvent {
    /// Builds an envelope, stamping the current time and composing `name`.
    pub fn new(source: impl Into<String>, kind: impl Into<String>, payload: impl Into<Value>) -> Self {
        let source = source.into();
        let kind = kind.into();
        Self {
            name: format!("{source}.{kind}"),
            source,
            kind,
            payload: payload.into(),
            timestamp: now_ms(),
        }
    }
}

/// The broadcast bus.
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<NoisyEvent>,
}

impl EventBus {
    /// Creates a bus with the given channel capacity (clamped to
    /// [`MIN_BUS_CAPACITY`]).
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(MIN_BUS_CAPACITY));
        Self { tx }
    }

    /// Publishes an event. Fire-and-forget: returns `false` when there is
    /// nobody to deliver to, which is not an error condition.
    pub fn emit(&self, event: NoisyEvent) -> bool {
        if self.tx.receiver_count() == 0 {
            return false;
        }
        self.tx.send(event).is_ok()
    }

    /// Number of current receivers.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Raw subscription handle for low-level pumps.
    pub fn subscribe(&self) -> broadcast::Receiver<NoisyEvent> {
        self.tx.subscribe()
    }

    /// Subscribes with pattern filtering and lag handling (see
    /// [`EventStream`]).
    pub fn stream(&self, pattern: impl Into<String>, subscriber: impl Into<String>) -> EventStream {
        EventStream {
            rx: self.subscribe(),
            bus: self.clone(),
            subscriber: subscriber.into(),
            pattern: pattern.into(),
            lag_reported: false,
        }
    }
}

/// Pattern-matched, lag-aware event stream connected by
/// [`crate::HostCtx::on`].
///
/// Patterns come in three shapes:
/// `"*"` — everything; `"net."` — every event of the `net` module;
/// `"net.disconnected"` — exactly that event.
#[derive(Debug)]
pub struct EventStream {
    rx: broadcast::Receiver<NoisyEvent>,
    bus: EventBus,
    subscriber: String,
    pattern: String,
    lag_reported: bool,
}

impl EventStream {
    /// Pattern match rule described in the type docs.
    pub fn matches(&self, event: &NoisyEvent) -> bool {
        event_matches(&self.pattern, event)
    }

    /// Subscribed pattern.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Next matching event. A lag burst produces at most one
    /// `noisy.bus_lagged { subscriber, missed }` warning (emitting during
    /// catch-up would amplify the very lag being recovered from), and the
    /// receiver resumes from the latest event. Returns `None` when the
    /// bus closed.
    pub async fn next(&mut self) -> Option<NoisyEvent> {
        loop {
            match self.rx.recv().await {
                Ok(event) => {
                    if self.matches(&event) {
                        self.lag_reported = false;
                        return Some(event);
                    }
                }
                Err(RecvError::Lagged(missed)) => {
                    if !self.lag_reported {
                        self.lag_reported = true;
                        self.bus.emit(NoisyEvent::new(
                            "noisy",
                            "bus_lagged",
                            json!({ "subscriber": self.subscriber, "missed": missed }),
                        ));
                    }
                }
                Err(RecvError::Closed) => return None,
            }
        }
    }
}

/// Implements the pattern rules.
pub fn event_matches(pattern: &str, event: &NoisyEvent) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('.') {
        return event.source == prefix;
    }
    event.name == pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emits_and_delivers_events() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        let delivered = bus.emit(NoisyEvent::new("serial", "data_received", json!({ "port": "/dev/tty0" })));
        assert!(delivered);

        let event = rx.recv().await.unwrap();
        assert_eq!(event.source, "serial");
        assert_eq!(event.kind, "data_received");
        assert_eq!(event.name, "serial.data_received");
        assert_eq!(event.payload["port"], "/dev/tty0");
        assert!(event.timestamp > 0);
    }

    #[tokio::test]
    async fn emit_without_receivers_is_a_no_op() {
        let bus = EventBus::new(4);
        assert!(!bus.emit(NoisyEvent::new("a", "b", json!({}))));
    }

    #[tokio::test]
    async fn lagged_subscriber_warns_and_resumes() {
        let bus = EventBus::new(2);
        let mut lagged_rx = bus.stream("blip.", "tester");
        let mut watcher_rx = bus.stream("noisy.bus_lagged", "watcher");

        for n in 0..6 {
            bus.emit(NoisyEvent::new("blip", "hit", json!({ "n": n })));
        }

        // lagged subscriber receives the latest matching event
        let first = lagged_rx.next().await.unwrap();
        assert_eq!(first.name, "blip.hit");
        assert_eq!(first.payload["n"], 5);

        // and the warning event reached the watcher
        let warning = watcher_rx.next().await.unwrap();
        assert_eq!(warning.source, "noisy");
        assert_eq!(warning.kind, "bus_lagged");
        assert_eq!(warning.payload["subscriber"], "tester");
        assert_eq!(warning.payload["missed"], 4);
    }

    #[tokio::test]
    async fn pattern_rules() {
        let bus = EventBus::new(8);
        let mut stream = bus.stream("net.", "tester");

        bus.emit(NoisyEvent::new("serial", "data_received", json!({})));
        bus.emit(NoisyEvent::new("net", "disconnected", json!({})));

        let event = stream.next().await.unwrap();
        assert_eq!(event.name, "net.disconnected");

        assert!(event_matches("*", &NoisyEvent::new("a", "b", json!({}))));
        assert!(event_matches("a.b", &NoisyEvent::new("a", "b", json!({}))));
        assert!(!event_matches("a.b", &NoisyEvent::new("a", "c", json!({}))));
    }
}
