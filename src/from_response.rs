use std::sync::Arc;
use crate::response::Response;

/// Various functions make use of this to determine what value to
/// return to the user. See [`Raw`] and [`De<T>`].
pub trait FromResponse {
    /// The output we'll hand back if all is well.
    type Output;
    /// The error we'll hand back if things go wrong.
    type Error: std::error::Error;
    /// Take a response and return a result.
    fn from_response(response: Arc<Response<'static>>) -> Result<Self::Output, Self::Error>;
}

/// Ask for the raw response object. This is cheaply cloneable,
/// and provides a reference to the [`Response`] returned without
/// doing any further deserializing.
#[derive(Clone, Debug)]
pub struct Raw(Arc<Response<'static>>);

/// This cannot happen.
#[derive(derive_more::Display, Debug)]
pub enum RawError {}

impl std::error::Error for RawError {}

impl Raw {
    /// Return a reference to the raw response.
    pub fn response(&self) -> &Response<'static> {
        &self.0
    }
}

impl std::ops::Deref for Raw {
    type Target = Response<'static>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FromResponse for Raw {
    type Output = Self;
    type Error = RawError;
    fn from_response(response: Arc<Response<'static>>) -> Result<Self::Output, Self::Error> {
        Ok(Self(response))
    }
}

/// Ask to deserialize a successful response "result" into some type that
/// implements [`serde::de::DeserializeOwned`], returning an error if either
/// a JSON-RPC error was returned, or if the deserializing fails.
pub struct De<T>(std::marker::PhantomData<T>);

/// An error returned from trying to deserialize the response.
#[derive(derive_more::Display, Debug)]
pub enum DeError {
    /// A JSON-RPC error object was returned. This error is emitted
    /// if you ask to deserialize some response, but get an error
    /// back instead of the expected response. We hand back the entire
    /// raw response to avoid extra allocations.
    #[display(fmt = "A JSON-RPC error was returned, so the response could not be deserialized")]
    Rpc(Raw),
    /// An error deserializing some received JSON into the expected
    /// JSON-RPC format.
    Deserialize(serde_json::Error),
}

impl std::error::Error for DeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DeError::Rpc(_) => None,
            DeError::Deserialize(e) => Some(e)
        }
    }
}

impl <T: serde::de::DeserializeOwned> FromResponse for De<T> {
    type Output = T;
    type Error = DeError;
    fn from_response(response: Arc<Response<'static>>) -> Result<Self::Output, Self::Error> {
        let res = match &*response {
            Response::Ok(res) => res,
            Response::Err(_) => return Err(DeError::Rpc(Raw(response)))
        };

        serde_json::from_str(res.result.get()).map_err(DeError::Deserialize)
    }
}