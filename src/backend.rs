use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Something that can be handed bytes to send out.
pub trait BackendSender: Send + Sync + 'static {
    /// Send a message to the JSON-RPC server, emitting an error if something goes wrong.
    /// The message should be serializable to a valid JSON-RPC object.
    fn send(&self, data: &[u8]) -> Pin<Box<dyn Future<Output = Result<(), BackendError>>>>;
}

/// Something that can receive bytes.
pub trait BackendReceiver {
    /// Hand back the next message each time it's called. If this emits a [`BackendError`], we'll
    /// stop asking for messages. The bytes given back should deserialize to a valid JSON-RPC object.
    fn receive(&self) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, BackendError>>>>;
}

/// An error that can occur from the backend.
pub type BackendError = Arc<dyn std::error::Error + Send + Sync + 'static>;
