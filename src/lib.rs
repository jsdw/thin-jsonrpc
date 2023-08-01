/// Helpers to build parameters for a JSON-RPC request.
pub mod params;
/// The backend trait, to connect a client to some server.
pub mod backend;
/// JSON-RPC response types.
pub mod response;

/// A broadcast-style stream of decoded JSON-RPC responses.
mod response_stream;

use futures::Future;
use response_stream::{ResponseStream, ResponseStreamMaster, ResponseStreamHandle};
use backend::{BackendSender, BackendReceiver};
use serde::de::DeserializeOwned;
use params::IntoRpcParams;
use std::sync::atomic::{ AtomicU64, Ordering };
use std::sync::Arc;

use crate::response::Response;

/// An error indicating that something went wrong trying to
/// send or receive messages.
#[derive(derive_more::From, derive_more::Display, Clone)]
pub enum Error {
    #[from]
    Backend(backend::BackendError),
    #[from]
    Response(response::ResponseError),
    #[display(fmt = "The connection has been closed")]
    ConnectionClosed
}

/// An error handed back from [`Client::request()`]. This can be
/// the error object handed back in the valid RPC response, or some
/// error deserializing the response to the requested type, or any
/// other [`Error`].
#[derive(derive_more::From, derive_more::Display)]
pub enum RequestError {
    #[from]
    Rpc(response::ErrorObject<'static>),
    #[from]
    Deserialize(serde_json::Error),
    #[from]
    Other(Error)
}

pub struct Client {
    next_id: AtomicU64,
    sender: Arc<dyn BackendSender>,
    stream: ResponseStreamHandle,
}

impl Client {
    /// Construct a client/driver from a [`BackendSender`] and [`BackendReceiver`].
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
            let Ok(msg) = res else {
                return false
            };

            let msg_id = match msg {
                Response::Err(err) => &err.id,
                Response::Ok(ok) => &ok.id
            };

            let Some(msg_id) = msg_id else {
                return false
            };

            id == &**msg_id
        }));

        // Now we're set up to wait for the reply, send the message.
        self.sender
            .send(request.as_bytes())
            .await
            .map_err(|e| RequestError::Other(e.into()))?;

        // Get the response.
        use futures::stream::StreamExt;
        let response = response_stream.next().await;

        match response {
            // Some message came back. if it's a response, deserialize it. If it's an error,
            // just return the error.
            Some(Ok(msg)) => {
                match &*msg {
                    Response::Ok(msg) => {
                        serde_json::from_str(msg.result.get()).map_err(|e| e.into())
                    },
                    Response::Err(e) => {
                        Err(RequestError::Rpc(e.error.clone()))
                    }
                }
            },
            // Report back any "show stopper" errors as-is.
            Some(Err(e)) => {
                Err(RequestError::Other(e.into()))
            },
            // None means that the stream is finished; connection closed. We'll have received
            // an error before this happens.
            None => {
                Err(RequestError::Other(Error::ConnectionClosed))
            }
        }
    }

    /// Send a notification to the RPC server. This will not return a result.
    pub async fn notification(&self, call: Call<'_>) -> Result<(), Error> {
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
    pub fn server_notifications(&self) -> ServerNotifications {
        todo!()
    }
}

/// This must be polled/`.await`-ed in order to accept messages from the server.
/// Nothing will happen unless it is. This design allows us to apply backpressure
/// (by polling this less often), and allows us to drive multiple notification streams
/// without requiring any specific async runtime.
pub struct ClientDriver(ResponseStreamMaster);

impl Future for ClientDriver {
    type Output = <ResponseStreamMaster as Future>::Output;
    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        use futures::FutureExt;
        self.0.poll_unpin(cx)
    }
}

/// Construct an RPC call. This can be used with [`Client::request()`] or [`Client::notification`].
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


pub struct ServerNotifications(ResponseStream);

