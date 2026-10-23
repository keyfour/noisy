//! Language-native callback routing (specification §2.3, §3.1).
//!
//! A Lua function passed as a method argument is wrapped by the binding
//! layer as an opaque token; modules invoke it with
//! [`crate::HostCtx::invoke_callback`], which queues a thread-safe job on
//! the Lua dispatch thread. Token ids are allocated by the binding layer
//! ([`CallbackHub::allocate_token`]); the Lua runtime registers itself as
//! the global sink and routes every unclaimed token, so the same mechanism
//! serves `timer.after(ms, fn)`-style APIs in every language.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// An opaque token identifying a language-native handler.
pub type CallbackToken = u64;

/// A callback invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct CallbackEnvelope {
    /// The token the invocation belongs to.
    pub token: CallbackToken,
    /// Payload passed by the caller.
    pub payload: Value,
}

#[derive(Debug, Default)]
struct HubInner {
    next_token: CallbackToken,
    /// Explicit per-token receivers (tests, engine-internal stubs).
    per_token: HashMap<CallbackToken, mpsc::Sender<CallbackEnvelope>>,
    /// The production consumer: the Lua dispatch thread inbox.
    sink: Option<mpsc::Sender<CallbackEnvelope>>,
}

/// Registry of language-native callbacks.
#[derive(Debug, Clone, Default)]
pub struct CallbackHub {
    inner: Arc<Mutex<HubInner>>,
}

impl CallbackHub {
    /// Creates an empty hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocates a fresh, unique token. The binding layer calls this when a
    /// Lua function argument crosses into a method call.
    pub fn allocate_token(&self) -> CallbackToken {
        let mut inner = self.inner.lock().unwrap();
        inner.next_token += 1;
        inner.next_token
    }

    /// Registers an explicit receiver for `token` (test/stub path); takes
    /// precedence over the global sink.
    pub fn register(&self, token: CallbackToken) -> mpsc::Receiver<CallbackEnvelope> {
        let (tx, rx) = mpsc::channel(8);
        self.inner.lock().unwrap().per_token.insert(token, tx);
        rx
    }

    /// Installs the global sink used for every token without an explicit
    /// receiver — the Lua dispatch thread inbox.
    pub fn set_sink(&self, sink: mpsc::Sender<CallbackEnvelope>) {
        self.inner.lock().unwrap().sink = Some(sink);
    }

    /// Removes a token's explicit receiver.
    pub fn unregister(&self, token: CallbackToken) {
        self.inner.lock().unwrap().per_token.remove(&token);
    }

    /// Invokes the handler behind `token` (non-blocking send). Returns
    /// `false` when the token is unknown or its receiver is gone.
    pub fn invoke(&self, token: CallbackToken, payload: impl Into<Value>) -> bool {
        let envelope = CallbackEnvelope {
            token,
            payload: payload.into(),
        };
        let inner = self.inner.lock().unwrap();
        match inner.per_token.get(&token) {
            Some(tx) => tx.try_send(envelope).is_ok(),
            None => match &inner.sink {
                Some(tx) => tx.try_send(envelope).is_ok(),
                None => false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn routes_invocations_to_registered_tokens() {
        let hub = CallbackHub::new();
        let mut rx = hub.register(7);

        assert!(hub.invoke(7, json!({ "n": 1 })));
        assert!(!hub.invoke(9, json!({})));

        let envelope = rx.recv().await.unwrap();
        assert_eq!(envelope.token, 7);
        assert_eq!(envelope.payload["n"], 1);

        hub.unregister(7);
        assert!(!hub.invoke(7, json!({})));
    }

    #[tokio::test]
    async fn unclaimed_tokens_flow_to_the_sink() {
        let hub = CallbackHub::new();
        let (tx, mut rx) = mpsc::channel(8);
        hub.set_sink(tx);

        let t1 = hub.allocate_token();
        let t2 = hub.allocate_token();
        assert_ne!(t1, t2);

        assert!(hub.invoke(t1, json!({ "hello": "world" })));
        let envelope = rx.recv().await.unwrap();
        assert_eq!(envelope.token, t1);
        assert_eq!(envelope.payload["hello"], "world");
        assert!(t2 > t1);

        // explicit receivers still win over the sink
        let mut explicit = hub.register(t2);
        assert!(hub.invoke(t2, json!({})));
        assert!(explicit.recv().await.is_some());
        assert!(rx.try_recv().is_err());
    }
}
