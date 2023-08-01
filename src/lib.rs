/// Helpers to build parameters for a JSON-RPC request.
pub mod params;
/// The backend trait, to connect a client to some server.
pub mod backend;
/// JSON-RPC response types.
pub mod response;

/// A broadcast-style stream of decoded JSON-RPC responses.
mod response_stream;

use futures::{Stream, StreamExt};
use response_stream::{ResponseStream, ResponseStreamMaster, ResponseStreamHandle};
use response::{Response};
use backend::{BackendSender, BackendReceiver};
use serde::de::DeserializeOwned;
use params::IntoRpcParams;
use std::sync::atomic::{ AtomicU64, Ordering };
use std::sync::Arc;

/// An error handed back from [`Client::request()`].
#[derive(Debug, derive_more::From, derive_more::Display)]
pub enum RequestError {
    #[from]
    Backend(backend::BackendError),
    #[from]
    Rpc(response::ErrorObject<'static>),
    #[from]
    Deserialize(serde_json::Error),
    ConnectionClosed,
}

impl std::error::Error for RequestError {}

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
    pub async fn request<Res>(&self, call: Call<'_>) -> Result<Res, RequestError>
    where
        Res: DeserializeOwned,
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
        let mut response_stream = self.stream.with_filter(Box::new(move |res| {
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
            // Some message came back. if it's a response, deserialize it. If it's an error,
            // just return the error.
            Some(msg) => {
                match &*msg {
                    Response::Ok(msg) => {
                        serde_json::from_str(msg.result.get()).map_err(|e| e.into())
                    },
                    Response::Err(e) => {
                        Err(RequestError::Rpc(e.error.clone()))
                    }
                }
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

    /// Obtain a stream of incoming notifications from the backend that aren't linked to
    /// any specific request.
    pub fn incoming(&self) -> ServerNotifications {
        todo!()
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
    /// Return all valid notifications.
    pub fn all(&self) -> ServerNotificationStream {
        let f: response_stream::FilterFn = Box::new(move |res| {
            // Always Ignore responses to requests:
            res.id().is_none()
        });

        ServerNotificationStream(self.0.with_filter(f))
    }

    /// Filter the incoming stream of notifications and only return
    /// matching messages. The filter is applied early on and can
    /// save allocations in the case that nothing is interested in a
    /// particular message.
    pub fn with_filter<F>(&self, mut filter_fn: F) -> ServerNotificationStream
    where
        F: FnMut(&Response<'_>) -> bool + Send + 'static
    {
        let f: response_stream::FilterFn = Box::new(move |res| {
            // Always Ignore responses to requests:
            if res.id().is_some() {
                return false
            }

            // Apply user filter:
            filter_fn(res)
        });

        ServerNotificationStream(self.0.with_filter(f))
    }
}

/// A stream of server notifications.
pub struct ServerNotificationStream(ResponseStream);

impl Stream for ServerNotificationStream {
    type Item = Arc<Response<'static>>;

    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_next_unpin(cx)
    }
}