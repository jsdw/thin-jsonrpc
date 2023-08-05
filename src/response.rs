use std::sync::Arc;
use crate::raw_response::RawResponse;

/// A JSON-RPC response.
#[derive(Debug, Clone)]
pub struct Response(Arc<RawResponse<'static>>);

/// An error returned from trying to deserialize the response.
#[derive(derive_more::Display, Debug)]
pub enum ResponseError {
    /// A JSON-RPC error object was returned. and so the "ok"
    /// value could not be deserialized.
    #[display(fmt = "A JSON-RPC error was returned, so the response could not be deserialized")]
    RpcErrorReturned,
    /// An error deserializing some received JSON into the expected
    /// JSON-RPC format.
    Deserialize(serde_json::Error),
}

impl Response {
    pub(crate) fn new(res: Arc<RawResponse<'static>>) -> Self {
        Self(res)
    }

    /// Hand back the raw response from the server.
    pub fn raw(&self) -> &RawResponse<'static> {
        &self.0
    }

    /// Deserialize an "ok" response to the requested type.
    pub fn ok_into<R: serde::de::DeserializeOwned>(&self) -> Result<R, ResponseError> {
        let res = match self.raw() {
            RawResponse::Ok(res) => res,
            RawResponse::Error(_) => return Err(ResponseError::RpcErrorReturned)
        };

        serde_json::from_str(res.result.get()).map_err(ResponseError::Deserialize)
    }
}
