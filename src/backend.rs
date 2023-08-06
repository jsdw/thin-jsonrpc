use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Something that can be handed bytes to send out. WHen every copy of the [`crate::Client`]
/// is dropped, this will be too.
pub trait BackendSender: Send + Sync + 'static {
    /// Send a message to the JSON-RPC server, emitting an error if something goes wrong.
    /// The message should be serializable to a valid JSON-RPC object.
    fn send(&self, data: &[u8]) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'static>>;
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
        #[serde(default = "empty_raw_value")]
        pub params: Box<serde_json::value::RawValue>
    }

    fn empty_raw_value() -> Box<serde_json::value::RawValue> {
        serde_json::value::RawValue::from_string("null".to_owned()).unwrap()
    }

    #[derive(Default)]
    struct MockBackendInner {
        handlers: HashMap<&'static str, Arc<dyn Fn(MockBackend, MockRequest) + Send + Sync + 'static>>,
        stopped: bool,
        send_waker: Option<std::task::Waker>,
        send_queue: VecDeque<Vec<u8>>,
    }

    impl MockBackend {
        /// Add a handler.
        pub fn handler<F>(&self, name: &'static str, callback: F) -> &Self
        where F: Fn(MockBackend, MockRequest) + Send + Sync + 'static
        {
            self.inner.lock().unwrap().handlers.insert(name, Arc::new(callback));
            self
        }

        /// Send an OK notification back
        pub fn send_ok_notification<V: serde::Serialize>(&self, value: V) {
            let res = crate::RawResponse::ok_from_value::<_, u8>(None, value);
            self.send_response(res)
        }

        /// Send an OK response back
        pub fn send_ok_response<Id: ToString, V: serde::Serialize>(&self, id: Option<Id>, value: V) {
            let res = crate::RawResponse::ok_from_value(id, value);
            self.send_response(res)
        }

        /// Send a [`crate::RawResponse`] back.
        pub fn send_response(&self, response: crate::RawResponse<'_>) {
            let res = serde_json::to_string(&response).unwrap();
            self.send_bytes(res.into_bytes())
        }

        /// Send raw bytes to be received by the [`BackendReceiver`].
        pub fn send_bytes(&self, bytes: Vec<u8>) {
            let mut inner = self.inner.lock().unwrap();
            if inner.stopped {
                return
            }
            inner.send_queue.push_back(bytes);
            if let Some(waker) = inner.send_waker.take() {
                waker.wake();
            }
        }

        /// Shut the backend down. It'll return None when asked for
        /// messages as soon as any existing queue has been drained.
        pub fn shutdown(&self) {
            let mut inner = self.inner.lock().unwrap();
            inner.stopped = true;
            if let Some(waker) = inner.send_waker.take() {
                waker.wake();
            }
        }
    }

    impl Drop for MockBackendSender {
        fn drop(&mut self) {
            MockBackend { inner: self.inner.clone() }.shutdown();
        }
    }

    impl BackendSender for MockBackendSender {
        fn send(&self, data: &[u8]) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'static>> {
            let Ok(req) = serde_json::from_slice::<MockRequest>(data) else {
                eprintln!("Error decoding: {}", std::str::from_utf8(data).unwrap());
                return Box::pin(std::future::ready(Ok(())))
            };

            let mock_backend = MockBackend { inner: self.inner.clone() };

            // Complain if not 2.0
            if req.jsonrpc != "2.0" {
                mock_backend.send_response(RawResponse::err_from_value(req.id, ErrorObject {
                    code: crate::raw_response::CODE_PARSE_ERROR,
                    message: format!("\"jsonrpc\" field was not equal to \"2.0\", was {}", req.jsonrpc).into(),
                    data: None
                }));
                return Box::pin(std::future::ready(Ok(())))
            }

            // Acquire lock only long enough to ge a copy our our handler, so we don't deadlock
            // trying to call the handler or send some other response (which will also lock).
            let maybe_handler = self.inner.lock().unwrap().handlers.get(req.method.as_str()).cloned();

            if let Some(handler) = maybe_handler {
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
                if let Some(item) = inner.send_queue.pop_front() {
                    Poll::Ready(Some(Ok(item)))
                } else if inner.stopped {
                    Poll::Ready(None)
                } else {
                    inner.send_waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }))
        }
    }

}