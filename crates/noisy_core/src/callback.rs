//! Language-native callback routing (specification §2.3, §3.1).
//!
//! A Lua function passed as a method argument is wrapped by the binding
//! layer as an opaque token; modules invoke it with
//! [`crate::HostCtx::invoke_callback`], which queues a thread-safe job on
//! the Lua dispatch thread. Phase 1 ships the routing primitive — the Lua
//! runtime (later phase) is the production subscriber; tests register
//! plain receivers.

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

/// Registry of language-native callbacks.
#[derive(Debug, Clone, Default)]
pub struct CallbackHub {
    inner: Arc<Mutex<HashMap<CallbackToken, mpsc::Sender<CallbackEnvelope>>>>,
}

impl CallbackHub {
    /// Creates an empty hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a receiver for `token`; invocations are delivered there.
    pub fn register(&self, token: CallbackToken) -> mpsc::Receiver<CallbackEnvelope> {
        let (tx, rx) = mpsc::channel(8);
        self.inner.lock().unwrap().insert(token, tx);
        rx
    }

    /// Removes a previously registered token.
    pub fn unregister(&self, token: CallbackToken) {
        self.inner.lock().unwrap().remove(&token);
    }

    /// Invokes the handler behind `token` (non-blocking send). Returns
    /// `false` when the token is unknown or its receiver is gone.
    pub fn invoke(&self, token: CallbackToken, payload: impl Into<Value>) -> bool {
        match self.inner.lock().unwrap().get(&token) {
            Some(tx) => tx
                .try_send(CallbackEnvelope {
                    token,
                    payload: payload.into(),
                })
                .is_ok(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn routes_invocations_to_registered_tokens() {
        let hub = CallbackHub::new();
        let mut rx = hub.register(7);

        assert!(hub.invoke(7, serde_json::json!({ "n": 1 })));
        assert!(!hub.invoke(9, serde_json::json!({})));

        let envelope = rx.recv().await.unwrap();
        assert_eq!(envelope.token, 7);
        assert_eq!(envelope.payload["n"], 1);

        hub.unregister(7);
        assert!(!hub.invoke(7, serde_json::json!({})));
    }
}
