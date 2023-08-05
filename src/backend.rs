use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Something that can be handed bytes to send out. There is one copy of this
/// that is not shared, and when our [`crate::Client`] is dropped, this will be too.
pub trait BackendSender: Send + 'static {
    /// Send a message to the JSON-RPC server, emitting an error if something goes wrong.
    /// The message should be serializable to a valid JSON-RPC object.
    fn send(&mut self, data: &[u8]) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'static>>;
}

/// Something that can receive bytes.
pub trait BackendReceiver: Send + 'static {
    /// Hand back the next message each time it's called. If this emits a [`BackendError`], we'll
    /// stop asking for messages. If it emits `None`, then the connection has been closed gracefully.
    /// The bytes given back should deserialize to a valid JSON-RPC object.
    fn receive(&self) -> Pin<Box<dyn Future<Output = Option<Result<Vec<u8>, BackendError>>> + Send + 'static>>;
}

/// An error that can occur from the backend.
pub type BackendError = Arc<dyn std::error::Error + Send + Sync + 'static>;

/// A mock backend which can be used in tests or examples.
pub mod mock {
    use crate::raw_response::ErrorObject;
    use crate::raw_response::RawResponse;

    use super::*;
    use std::task::Poll;
    use std::collections::VecDeque;
    use std::collections::HashMap;

    /// Construct a new mock backend.
    pub fn build() -> (MockBackend, MockBackendSender, MockBackendReceiver) {
        let inner = Default::default();

        (
            MockBackend {inner: Arc::clone(&inner)},
            MockBackendSender { inner: Arc::clone(&inner)},
            MockBackendReceiver { inner }
        )
    }

    /// A mock backend.
    #[derive(Clone)]
    pub struct MockBackend {
        inner: Arc<std::sync::Mutex<MockBackendInner>>
    }

    /// The sender half of the mock backend.
    pub struct MockBackendSender {
        inner: Arc<std::sync::Mutex<MockBackendInner>>
    }

    /// The receiver half of the mock backend.
    pub struct MockBackendReceiver {
        inner: Arc<std::sync::Mutex<MockBackendInner>>
    }

    /// A mock request
    #[allow(missing_docs)]
    #[derive(serde::Deserialize, Debug, Clone)]
    pub struct MockRequest {
        pub jsonrpc: String,
        pub id: Option<String>,
        pub method: String,
        pub params: Box<serde_json::value::RawValue>
    }

    #[derive(Default)]
    struct MockBackendInner {
        handlers: HashMap<&'static str, Box<dyn Fn(MockBackend, MockRequest) + Send + 'static>>,
        sender_dropped: bool,
        send_waker: Option<std::task::Waker>,
        send_queue: VecDeque<Vec<u8>>,
    }

    impl MockBackend {
        /// Add a handler.
        pub fn handler<F>(&self, name: &'static str, callback: F) -> &Self
        where F: Fn(MockBackend, MockRequest) + Send + 'static
        {
            self.inner.lock().unwrap().handlers.insert(name, Box::new(callback));
            self
        }

        /// Send a [`crate::Response`] back.
        pub fn send_response(&self, response: crate::RawResponse<'_>) {
            let res = serde_json::to_string(&response).unwrap();
            self.send_bytes(res.into_bytes())
        }

        /// Send raw bytes to be received by the [`BackendReceiver`].
        pub fn send_bytes(&self, bytes: Vec<u8>) {
            let mut inner = self.inner.lock().unwrap();
            inner.send_queue.push_back(bytes);
            if let Some(waker) = inner.send_waker.take() {
                println!("WAKER WAKING");
                waker.wake();
            }
        }
    }

    impl Drop for MockBackendSender {
        fn drop(&mut self) {
            if let Ok(mut inner) = self.inner.lock() {
                inner.sender_dropped = true;
                // We've dropped the BackendSender, which signals to the backend
                // to shut down. So, any messages we were going to send
                // to the client are now thrown away.
                inner.send_queue = VecDeque::new();
                // Wake the receiver so it can collect a `None` and see that
                // things have shut down.
                if let Some(waker) = inner.send_waker.take() {
                    waker.wake();
                }
            }
        }
    }

    impl BackendSender for MockBackendSender {
        fn send(&mut self, data: &[u8]) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'static>> {
            let Ok(req) = serde_json::from_slice::<MockRequest>(data) else {
                return Box::pin(std::future::ready(Ok(())))
            };

            let mock_backend = MockBackend { inner: self.inner.clone() };
            let inner = self.inner.lock().unwrap();

            if req.jsonrpc != "2.0" {
                mock_backend.send_response(RawResponse::err_from_value(req.id, ErrorObject {
                    code: crate::raw_response::CODE_PARSE_ERROR,
                    message: format!("\"jsonrpc\" field was not equal to \"2.0\", was {}", req.jsonrpc).into(),
                    data: None
                }));
            } else if let Some(handler) = inner.handlers.get(req.method.as_str()) {
                handler(mock_backend, req);
            } else {
                mock_backend.send_response(RawResponse::err_from_value(req.id, ErrorObject {
                    code: crate::raw_response::CODE_METHOD_NOT_FOUND,
                    message: format!("No method named '{}'", req.method).into(),
                    data: None
                }));
            }

            Box::pin(std::future::ready(Ok(())))
        }
    }

    impl BackendReceiver for MockBackendReceiver {
        fn receive(&self) -> Pin<Box<dyn Future<Output = Option<Result<Vec<u8>, BackendError>>> + Send + 'static>> {
            let inner = self.inner.clone();
            Box::pin(std::future::poll_fn(move |cx| {
                let mut inner = inner.lock().unwrap();
                if inner.sender_dropped {
                    Poll::Ready(None)
                } else if let Some(item) = inner.send_queue.pop_front() {
                    Poll::Ready(Some(Ok(item)))
                } else {
                    inner.send_waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }))
        }
    }

}