//! # thin-jsonrpc-client
//!
//! This crate provides a lightweight JSON-RPC compatible client.
#![deny(missing_docs)]

/// A broadcast-style stream of decoded JSON-RPC responses.
mod response_stream;

/// Helpers to build parameters for a JSON-RPC request.
pub mod params;
/// The backend trait, to connect a client to some server.
pub mod backend;
/// JSON-RPC response types.
pub mod response;
/// Decide how you'd like back each response.
pub mod from_response;

use futures_core::Stream;
use futures_util::StreamExt;
use response_stream::{ResponseStream, ResponseStreamMaster, ResponseStreamHandle};
use response::{Response};
use backend::{BackendSender, BackendReceiver};
use params::IntoRpcParams;
use std::sync::atomic::{ AtomicU64, Ordering };
use std::sync::Arc;
use std::task::Poll;
use from_response::FromResponse;

/// An error handed back from [`Client::request()`].
#[derive(Debug, derive_more::From, derive_more::Display)]
#[non_exhaustive]
pub enum RequestError<E> {
    /// An error from the backend implementation that was emitted when
    /// attempting to send the request.
    #[from]
    Backend(backend::BackendError),
    /// An error happened handling the response.
    Response(E),
    /// The connection was closed before a response was delivered.
    ConnectionClosed,
}

impl <E: std::error::Error + 'static> std::error::Error for RequestError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RequestError::Backend(e) => Some(e),
            RequestError::Response(e) => Some(e),
            RequestError::ConnectionClosed => None
        }
    }
}

/// A JSON-RPC client. Build this by calling [`Client::from_backend()`]
/// and providing a suitable sender and receiver.
pub struct Client {
    next_id: AtomicU64,
    sender: Arc<dyn BackendSender>,
    stream: ResponseStreamHandle,
}

impl Client {
    /// Construct a client/driver from a [`BackendSender`] and [`BackendReceiver`].
    /// The [`ClientDriver`] handed back is a stream which needs polling in order to
    /// drive the message receiving.
    pub fn from_backend<S, R>(send: S, recv: R) -> (Client, ClientDriver)
    where
        S: BackendSender,
        R: BackendReceiver + 'static
    {
        let master = ResponseStreamMaster::new(Box::new(recv));

        let client = Client {
            next_id: AtomicU64::new(1),
            sender: Arc::new(send),
            stream: master.handle()
        };
        let client_driver = ClientDriver(master);

        (client, client_driver)
    }

    /// Make a request to the RPC server. This will return either a response or an error,
    /// and will attempt to deserialize the response into the type you've asked for.
    pub async fn request<Res>(&self, call: Call<'_>) -> Result<Res::Output, RequestError<Res::Error>>
    where
        Res: FromResponse,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let call_name = call.name;
        let params = call.params;

        let request = match params {
            Some(params) =>
                format!("{{\"jsonrpc\":\"2.0\",\"id\":\"{id}\",\"method\":\"{call_name}\",\"params\":{params}}}"),
            None =>
                format!("{{\"jsonrpc\":\"2.0\",\"id\":\"{id}\",\"method\":\"{call_name}\"}}"),
        };

        // Subscribe to responses with the matching ID.
        let mut response_stream = self.stream.with_filter(Arc::new(move |res| {
            let Some(msg_id) = res.id() else {
                return false
            };

            id == msg_id
        }));

        // Now we're set up to wait for the reply, send the message.
        self.sender
            .send(request.as_bytes())
            .await
            .map_err(|e| RequestError::Backend(e))?;

        // Get the response.
        let response = response_stream.next().await;

        match response {
            // Some message came back.
            Some(msg) => {
                Res::from_response(msg).map_err(RequestError::Response)
            },
            // None means that the stream is finished; connection closed.
            None => {
                Err(RequestError::ConnectionClosed)
            }
        }
    }

    /// Send a notification to the RPC server. This will not return a result.
    pub async fn notification(&self, call: Call<'_>) -> Result<(), backend::BackendError> {
        let call_name = call.name;
        let params = call.params;

        let notification = match params {
            Some(params) =>
                format!("{{\"jsonrpc\":\"2.0\",\"method\":\"{call_name}\",\"params\":{params}}}"),
            None =>
                format!("{{\"jsonrpc\":\"2.0\",\"method\":\"{call_name}\"}}"),
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
/// without requiring any specific async runtime.
pub struct ClientDriver(ResponseStreamMaster);

/// An error driving the message receiving. If the stream subsequently
/// returns [`None`], it was due to the last emitted error.
pub type ClientDriverError = response_stream::ResponseStreamError;

impl Stream for ClientDriver {
    type Item = ClientDriverError;
    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_next_unpin(cx)
    }
}

/// Construct an RPC call. This can be used with [`Client::request()`] or [`Client::notification`].
#[derive(Debug, Clone)]
pub struct Call<'a> {
    name: &'a str,
    params: params::RpcParams
}

impl <'a> Call<'a> {
    /// Construct a new call given a name and some params. This can then
    /// be handed to [`Client::request()`] or [`Client::notification()`].
    pub fn new<Params: IntoRpcParams>(name: &'a str, params: Params) -> Self {
        Self {
            name,
            params: params.into_rpc_params()
        }
    }
}

/// A struct representing messages from the server.
pub struct ServerNotifications(ResponseStreamHandle);

impl ServerNotifications {
    /// Return any notifications not being handed to any other stream.
    /// This is useful for debugging and diagnostic purposes, because
    /// it's likely that every incoming message is expected to be handled
    /// by something. It's the equivalent of a "404" location.
    pub fn leftovers<Res: FromResponse>(&self) -> ServerNotificationStream<Res> {
        ServerNotificationStream::new(self.0.leftovers())
    }

    /// Return all valid notifications.
    pub fn all<Res: FromResponse>(&self) -> ServerNotificationStream<Res> {
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
    pub fn with_filter<Res, F>(&self, filter_fn: F) -> ServerNotificationStream<Res>
    where
        Res: FromResponse,
        F: Fn(&Response<'_>) -> bool + Send + 'static
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
pub struct ServerNotificationStream<Res> {
    stream: ResponseStream,
    _marker: std::marker::PhantomData<Res>
}

impl <Res> ServerNotificationStream<Res> {
    fn new(stream: ResponseStream) -> Self {
        Self {
            stream,
            _marker: std::marker::PhantomData
        }
    }
}

impl <Res: FromResponse> Stream for ServerNotificationStream<Res> {
    type Item = Result<Res::Output, Res::Error>;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        // Safety: I'm not moving anything around,
        // so the Pin promise is preserved.
        let this = unsafe { self.get_unchecked_mut() };
        this.stream.poll_next_unpin(cx).map(|o| o.map(Res::from_response))
    }
}
