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
use response::ErrorObject;
use response_stream::{ResponseStreamMaster, ResponseStreamHandle, ResponseStream};
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
#[derive(Clone)]
pub struct Client {
    next_id: Arc<AtomicU64>,
    sender: Arc<dyn BackendSender>,
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
            next_id: Arc::new(AtomicU64::new(1)),
            sender: Arc::new(send),
            stream: master.handle()
        };
        let client_driver = ClientDriver(master);

        (client, client_driver)
    }

    /// Make a request to the RPC server. This will return either a response or an error.
    pub async fn send_request<Params>(&self, method: &str, params: Params) -> Result<Response, RequestError>
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
        let mut response_stream = self.stream.response_stream().filter(move |res| {
            let Some(msg_id) = res.id() else {
                return std::future::ready(false)
            };

            std::future::ready(id == msg_id)
        });

        // Now we're set up to wait for the reply, send the request.
        self.sender
            .send(request.as_bytes())
            .await
            .map_err(|e| RequestError::Backend(e))?;

        // Get the response.
        let response = response_stream.next().await;

        match response {
            Some(res) => {
                Ok(Response(res))
            },
            None => {
                Err(RequestError::ConnectionClosed)
            }
        }
    }

    /// Send a notification to the RPC server. This will not wait for a response.
    pub async fn send_notification<Params>(&mut self, method: &str, params: Params) -> Result<(), backend::BackendError>
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
    pub fn notifications(&self) -> ServerNotifications {
        ServerNotifications(self.stream.response_stream())
    }
}

/// This must be polled in order to accept messages from the server.
/// Nothing will happen unless it is. This design allows us to apply backpressure
/// (by polling this less often), and allows us to drive multiple notification streams
/// without requiring any specific async runtime. It will return:
///
/// - `Some(Ok(response))` if it successfully received a response.
/// - `Some(Err(e))` if it failed to parse some bytes into a response, or the response
///    was invalid.
/// - `None` if the backend has stopped (either after a fatal error, which will be
///   delivered just prior to this, or because the [`Client`] was dropped.
pub struct ClientDriver(ResponseStreamMaster);

/// An error was encountered while receiving messages from the backend.
pub type ClientDriverError = response_stream::ResponseStreamError;

impl Stream for ClientDriver {
    type Item = Result<Response, ClientDriverError>;
    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_next_unpin(cx).map(|o| o.map(|r| r.map(Response)))
    }
}

/// A struct representing messages from the server. This
/// implements [`futures_util::Stream`] and provides a couple of
/// additional helper methods to further filter the stream.
pub struct ServerNotifications(ResponseStream);

impl ServerNotifications {
    /// This is analogous to [`Response::ok_into()`], but will apply to each
    /// notification in the stream, filtering out any that don't deserialize into
    /// the given type.
    pub fn ok_into<R>(self) -> impl Stream<Item=R> + Send + Sync + 'static
    where
        R: for<'de> serde::de::Deserialize<'de> + Send + Sync + 'static,
    {
        self.filter_map(move |res| {
            match res.ok_into() {
                Err(_) => std::future::ready(None),
                Ok(r) => std::future::ready(Some(r))
            }
        })
    }

    /// Like [`ServerNotifications::ok_into()`], but also accepts a filter function to
    /// ignore any values that we're not interested in.
    pub fn ok_into_if<R, F>(self, filter_fn: F) -> impl Stream<Item=R> + Send + Sync + 'static
    where
        R: for<'de> serde::de::Deserialize<'de> + Send + Sync + 'static,
        F: Fn(&R) -> bool + Send + Sync + 'static
    {
        self.ok_into().filter(move |n| std::future::ready(filter_fn(n)))
    }

    /// This is analogous to [`Response::error_into()`], but will apply to each
    /// notification in the stream, filtering out any that don't deserialize into
    /// the given type.
    pub fn error_into<R>(self) -> impl Stream<Item=ErrorObject<R>> + Send + Sync + 'static
    where
        R: for<'de> serde::de::Deserialize<'de> + Send + Sync + 'static,
    {
        self.filter_map(move |res| {
            match res.error_into() {
                Err(_) => std::future::ready(None),
                Ok(r) => std::future::ready(Some(r))
            }
        })
    }

    /// Like [`ServerNotifications::error_into()`], but also accepts a filter function to
    /// ignore any values that we're not interested in.
    pub fn error_into_if<R, F>(self, filter_fn: F) -> impl Stream<Item=ErrorObject<R>> + Send + Sync + 'static
    where
        R: for<'de> serde::de::Deserialize<'de> + Send + Sync + 'static,
        F: Fn(&ErrorObject<R>) -> bool + Send + Sync + 'static
    {
        self.error_into().filter(move |n| std::future::ready(filter_fn(n)))
    }
}


impl Stream for ServerNotifications {
    type Item = Response;
    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let Poll::Ready(res) = self.0.poll_next_unpin(cx) else {
                return Poll::Pending
            };

            let Some(res) = res else {
                return Poll::Ready(None)
            };

            if res.id().is_some() {
                // If the response has an ID, we filter
                // it out. Loop and poll again because we
                // can't return pending if the inner was ready.
                continue
            }

            return Poll::Ready(Some(Response(res)))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use backend::mock::{ self, MockBackend };
    use futures_util::StreamExt;
    use crate::response::ErrorObject;

    type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

    fn mock_client() -> (Client, ClientDriver, MockBackend) {
        let (mock_backend, mock_send, mock_recv) = mock::build();
        let (client, driver) = Client::from_backend(mock_send, mock_recv);
        (client, driver, mock_backend)
    }

    fn drive_client(mut driver: ClientDriver) {
        tokio::spawn(async move {
            while let Some(res) = driver.next().await {
                if let Err(err) = res {
                    eprintln!("ClientDriver Error: {err}");
                }
            }
        });
    }

    #[tokio::test]
    async fn test_basic_requests() -> Result<(), Error> {
        let (client, driver, backend) = mock_client();
        drive_client(driver);

        backend
            .handler("echo_ok_raw", |cx, req| {
                let id = req.id.unwrap_or("-1".to_string());
                let params = req.params;
                let res = format!(r#"{{ "jsonrpc": "2.0", "id": "{id}", "result": {params} }}"#);
                cx.send_bytes(res.into_bytes());
            })
            .handler("echo_err_raw", |cx, req| {
                let id = req.id.unwrap_or("-1".to_string());
                let params = req.params;
                let res = format!(r#"{{ "jsonrpc": "2.0", "id": "{id}", "error": {{ "code":123, "message":"Eep!", "data": {params} }} }}"#);
                cx.send_bytes(res.into_bytes());
            })
            .handler("add", |cx, req| {
                let (a, b): (i64, i64) = serde_json::from_str(req.params.get()).unwrap();
                cx.send_ok_response(req.id, a+b);
            });

        // Check we can decode ok response properly.
        let res: Vec<u8> = client.send_request("echo_ok_raw", (1,2,3)).await?.ok_into()?;
        assert_eq!(res, vec![1,2,3]);

        // Check we can decode error response properly.
        let err: ErrorObject<Vec<u8>> = client.send_request("echo_err_raw", (1,2,3)).await?.error_into()?;
        assert_eq!(err.code, 123);
        assert_eq!(err.message, "Eep!");
        assert_eq!(err.data, vec![1,2,3]);

        // Ensure ID's are incremented and such.
        for i in 0i64..500 {
            let res: i64 = client.send_request("add", (i, 1)).await?.ok_into()?;
            assert_eq!(res, i + 1);
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_notifications() -> Result<(), Error> {
        let (mut client, driver, backend) = mock_client();
        drive_client(driver);

        backend
            .handler("start_sending", |cx, _req| {
                for i in 0u64..20 {
                    cx.send_ok_notification(i);
                }
                // If we don't shutdown after, the streams will wait
                // forever for new messages:
                cx.shutdown();
            });

        // Subscribe to messages:
        let twos = client.notifications().ok_into_if(|n: &u64| n % 2 == 0);
        let threes = client.notifications().ok_into_if(|res: &u64| res % 3 == 0);
        let bools = client.notifications().ok_into::<bool>();

        // Start sending notifications.
        client.send_notification("start_sending", ()).await?;

        // Collect them in our streams to check:
        let twos: Vec<u64> = twos.collect().await;
        let threes: Vec<u64> = threes.collect().await;
        let bools: Vec<_> = bools.collect().await;

        let expected_twos = vec![0,2,4,6,8,10,12,14,16,18];
        let expected_threes = vec![0,3,6,9,12,15,18];

        assert_eq!(twos, expected_twos);
        assert_eq!(threes, expected_threes);
        assert!(bools.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_lots_of_subscriptions() {
        let (client, driver, backend) = mock_client();
        drive_client(driver);

        // lots of listeners:
        let handles = (0..1000).map(|_| {
            let mut notifs = client.notifications().ok_into::<u64>();
            tokio::spawn(async move {
                let mut n = 0;
                while let Some(_res) = notifs.next().await {
                    n += 1;
                }
                n
            })
        });

        tokio::spawn(async move {
            for n in 0u64..1000 {
                backend.send_ok_notification(n);
            }
            // Else streams will wait forever:
            backend.shutdown();
        });

        let counts: Vec<_> = futures_util::future::join_all(handles).await;

        // Check that every stream saw every message:
        for count in counts {
            assert_eq!(count.unwrap(), 1000);
        }
    }
}