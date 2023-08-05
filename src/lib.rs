//! # thin-jsonrpc-client
//!
//! This crate provides a lightweight JSON-RPC compatible client.
#![deny(missing_docs)]

/// A broadcast-style stream of decoded JSON-RPC responses.
mod response_stream;
mod response;

/// Helpers to build parameters for a JSON-RPC request.
pub mod params;
/// The backend trait, to connect a client to some server.
pub mod backend;
/// JSON-RPC response types.
pub mod raw_response;

use futures_core::Stream;
use futures_util::StreamExt;
use response_stream::{ResponseStream, ResponseStreamMaster, ResponseStreamHandle};
use raw_response::{RawResponse};
use backend::{BackendSender, BackendReceiver};
use params::IntoRpcParams;
use std::sync::atomic::{ AtomicU64, Ordering };
use std::sync::Arc;
use std::task::Poll;

pub use response::{ Response, ResponseError };

/// An error handed back from [`Client::request()`].
#[derive(Debug, derive_more::From, derive_more::Display)]
#[non_exhaustive]
pub enum RequestError {
    /// An error from the backend implementation that was emitted when
    /// attempting to send the request.
    #[from]
    Backend(backend::BackendError),
    /// The connection was closed before a response was delivered.
    ConnectionClosed,
}

impl std::error::Error for RequestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RequestError::Backend(e) => Some(e),
            RequestError::ConnectionClosed => None
        }
    }
}

/// A JSON-RPC client. Build this by calling [`Client::from_backend()`]
/// and providing a suitable sender and receiver.
pub struct Client {
    next_id: AtomicU64,
    sender: Box<dyn BackendSender>,
    stream: ResponseStreamHandle,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("next_id", &self.next_id)
            .field("sender", &"Box<dyn BackendSender>")
            .field("stream", &self.stream)
            .finish()
    }
}

impl Client {
    /// Construct a client/driver from a [`BackendSender`] and [`BackendReceiver`].
    /// The [`ClientDriver`] handed back is a stream which needs polling in order to
    /// drive the message receiving.
    pub fn from_backend<S, R>(send: S, recv: R) -> (Client, ClientDriver)
    where
        S: BackendSender,
        R: BackendReceiver
    {
        let master = ResponseStreamMaster::new(Box::new(recv));

        let client = Client {
            next_id: AtomicU64::new(1),
            sender: Box::new(send),
            stream: master.handle()
        };
        let client_driver = ClientDriver(master);

        (client, client_driver)
    }

    /// Make a request to the RPC server. This will return either a response or an error,
    /// and will attempt to deserialize the response into the type you've asked for.
    pub async fn request<Params>(&mut self, method: &str, params: Params) -> Result<Response, RequestError>
    where Params: IntoRpcParams
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();

        // Build the request:
        let request = match params.into_rpc_params() {
            Some(params) =>
                format!(r#"{{"jsonrpc":"2.0","id":"{id}","method":"{method}","params":{params}}}"#),
            None =>
                format!(r#"{{"jsonrpc":"2.0","id":"{id}","method":"{method}"}}"#),
        };

        // Subscribe to responses with the matching ID.
        let mut response_stream = self.stream.with_filter(Arc::new(move |res| {
            let Some(msg_id) = res.id() else {
                return false
            };

            id == msg_id
        }));

        // Now we're set up to wait for the reply, send the request.
        self.sender
            .send(request.as_bytes())
            .await
            .map_err(|e| RequestError::Backend(e))?;

        // Get the response.
        let response = response_stream.next().await;

        match response {
            Some(res) => {
                Ok(Response::new(res))
            },
            None => {
                Err(RequestError::ConnectionClosed)
            }
        }
    }

    /// Send a notification to the RPC server. This will not return a result.
    pub async fn notification<Params>(&mut self, method: &str, params: Params) -> Result<(), backend::BackendError>
    where Params: IntoRpcParams
    {
        let notification = match params.into_rpc_params() {
            Some(params) =>
                format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":{params}}}"#),
            None =>
                format!(r#"{{"jsonrpc":"2.0","method":"{method}"}}"#),
        };

        // Send the message. No need to wait for any response.
        self.sender.send(notification.as_bytes()).await?;
        Ok(())
    }

    /// Obtain a stream of server notifications from the backend that aren't linked to
    /// any specific request.
    pub fn server_notifications(&self) -> ServerNotifications {
        ServerNotifications(self.stream.clone())
    }
}

/// This must be polled in order to accept messages from the server.
/// Nothing will happen unless it is. This design allows us to apply backpressure
/// (by polling this less often), and allows us to drive multiple notification streams
/// without requiring any specific async runtime. It will return:
///
/// - `Some(Ok(()))` if it successfully received and delivered a message.
/// - `Some(Err(e))` if it failed to deliver a message for some reason.
/// - `None` if the backend has stopped (either after a fatal error, which will be
///   delivered just prior to this, or because the [`Client`] was dropped.
pub struct ClientDriver(ResponseStreamMaster);

/// An error driving the message receiving. If the stream subsequently
/// returns [`None`], it was due to the last emitted error.
pub type ClientDriverError = response_stream::ResponseStreamError;

impl Stream for ClientDriver {
    type Item = Result<(), ClientDriverError>;
    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_next_unpin(cx)
    }
}

/// A struct representing messages from the server.
pub struct ServerNotifications(ResponseStreamHandle);

impl ServerNotifications {
    /// Return any notifications not being handed to any other stream.
    /// This is useful for debugging and diagnostic purposes, because
    /// it's likely that every incoming message is expected to be handled
    /// by something. It's the equivalent of a "404" location.
    pub fn leftovers(&self) -> ServerNotificationStream {
        ServerNotificationStream::new(self.0.leftovers())
    }

    /// Return all valid notifications.
    pub fn all(&self) -> ServerNotificationStream {
        let f: response_stream::FilterFn = Arc::new(move |res| {
            // Always Ignore responses to requests:
            res.id().is_none()
        });

        ServerNotificationStream::new(self.0.with_filter(f))
    }

    /// Filter the incoming stream of notifications and only return
    /// matching messages. The filter is applied early on and can
    /// save allocations in the case that nothing is interested in a
    /// particular message.
    pub fn with_filter<F>(&self, filter_fn: F) -> ServerNotificationStream
    where
        F: Fn(&RawResponse<'_>) -> bool + Send + Sync + 'static
    {
        let f: response_stream::FilterFn = Arc::new(move |res| {
            // Always Ignore responses to requests:
            if res.id().is_some() {
                return false
            }

            // Apply user filter:
            filter_fn(res)
        });

        ServerNotificationStream::new(self.0.with_filter(f))
    }
}

/// A stream of server notifications.
pub struct ServerNotificationStream {
    stream: ResponseStream
}

impl ServerNotificationStream {
    fn new(stream: ResponseStream) -> Self {
        Self { stream }
    }
}

impl Stream for ServerNotificationStream {
    type Item = Response;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        // Safety: I'm not moving anything around,
        // so the Pin promise is preserved.
        let this = unsafe { self.get_unchecked_mut() };
        this.stream.poll_next_unpin(cx).map(|o| o.map(Response::new))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use backend::mock::{ self, MockBackend };
    use raw_response::RawResponse;
    use futures_util::StreamExt;

    fn mock_client() -> (Client, MockBackend) {
        let (mock_backend, mock_send, mock_recv) = mock::build();
        let (client, mut driver) = Client::from_backend(mock_send, mock_recv);
        tokio::spawn(async move {
            println!("HELLO!!")
        });
        // Drive the receipt of messages until a shutdown happens.
        tokio::spawn(async move {
            eprintln!("SPAWNED");
            while let Some(res) = driver.next().await {
                if let Err(err) = res {
                    eprintln!("ClientDriver Error: {err}");
                }
            }
        });
        (client, mock_backend)
    }

    #[tokio::test]
    async fn test_request() {
        let (mut client, backend) = mock_client();

        backend.handler("echo", |cx, req| {
            cx.send_response(RawResponse::ok_from_value(req.id, &req.params))
        });

        let server_notifications = client.server_notifications();
        tokio::spawn(async move {
            let mut leftovers = server_notifications.leftovers();
            while let Some(item) = leftovers.next().await {
                println!("Leftover: {item:?}");
            }
        });

        let res: Vec<u8> = client.request("echo", params![1,2,3]).await.unwrap().ok_into().unwrap();
        assert_eq!(res, vec![1,2,3]);
    }
}