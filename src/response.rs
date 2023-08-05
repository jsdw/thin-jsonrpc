use std::sync::Arc;
use crate::raw_response::RawResponse;

/// A JSON-RPC response.
#[derive(Debug, Clone)]
pub struct Response(pub(crate) Arc<RawResponse<'static>>);

/// An error returned from trying to deserialize the response.
#[derive(derive_more::Display, Debug)]
pub enum ResponseError {
    /// A JSON-RPC error object was returned. and so the "ok"
    /// value could not be deserialized.
    #[display(fmt = "An ok or error value was expected, and the other was received.")]
    WrongResponse,
    /// An error deserializing some received JSON into the expected
    /// JSON-RPC format.
    Deserialize(serde_json::Error),
}

impl std::error::Error for ResponseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ResponseError::WrongResponse => None,
            ResponseError::Deserialize(err) => Some(err)
        }
    }
}

impl Response {
    /// Hand back the raw response from the server.
    pub fn raw(&self) -> &RawResponse<'static> {
        &self.0
    }

    /// Deserialize an "ok" response to the requested type.
    pub fn ok_into<R: serde::de::DeserializeOwned>(&self) -> Result<R, ResponseError> {
        let res = match self.raw() {
            RawResponse::Ok(res) => res,
            RawResponse::Error(_) => return Err(ResponseError::WrongResponse)
        };

        serde_json::from_str(res.result.get()).map_err(ResponseError::Deserialize)
    }

    /// Deserialize an "error" response to the requested type.
    pub fn error_into<Data: serde::de::DeserializeOwned>(&self) -> Result<ErrorObject<Data>, ResponseError> {
        let err = match self.raw() {
            RawResponse::Error(err) => err,
            RawResponse::Ok(_) => return Err(ResponseError::WrongResponse)
        };

        let data = err.error.data.as_ref().map(|d| d.get()).unwrap_or("null");
        let data = serde_json::from_str(data).map_err(ResponseError::Deserialize)?;

        Ok(ErrorObject {
            code: err.error.code,
            message: err.error.message.as_ref().to_owned(),
            data
        })
    }
}

/// The deserialized error object returned from [`Response::err_into()`].
pub struct ErrorObject<Data> {
    pub code: i32,
    pub message: String,
    pub data: Data
}
