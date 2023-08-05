use serde_json::value::RawValue;
use std::borrow::Cow;

/// An error handed back when we cannot correctly deserialize
/// bytes into our [`Response`] object.
#[derive(Debug, derive_more::From, derive_more::Display)]
pub enum ResponseError {
    /// There was an error deserializing the response.
    #[from]
    #[display(fmt = "{error} ({:?})", "std::str::from_utf8(&bytes)")]
    Deserialize {
        /// The underlying serde error.
        error: serde_json::Error,
        /// The raw bytes that we failed to deserialize.
        bytes: Vec<u8>
    },
    /// The "jsonrpc" field did not equal "2.0".
    #[display(fmt = "failed to decode response: expected '\"jsonrpc\": \"2.0\"'")]
    InvalidVersion {
        /// The raw bytes that we failed to deserialize.
        bytes: Vec<u8>
    }
}

/// A JSON-RPC response is either a "result" or "error" payload.
/// This represents the shape a message can deserialize into.
// Dev note: We can't Deserialize things into this in a sane way,
// because we'd need #[serde(untagged)] + RawValue, which don't play
// well together.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(untagged)]
pub enum RawResponse<'a> {
    /// A JSON-RPC "result" response.
    Ok(OkResponse<'a>),
    /// A JSON-RPC "error" response.
    Error(ErrorResponse<'a>)
}

impl <'a> RawResponse<'a> {
    /// Return the ID associated with the response, if there is one.
    /// Notifications from the server that aren't associated with a
    /// request won't.
    pub fn id(&self) -> Option<&str> {
        match self {
            RawResponse::Ok(r) => r.id.as_deref(),
            RawResponse::Error(e) => e.id.as_deref()
        }
    }

    /// Decode some bytes into a valid JSON-RPC response or
    /// return an error if it's not valid.
    pub fn from_bytes(bytes: &[u8]) -> Result<RawResponse<'_>, ResponseError> {
        let res = match serde_json::from_slice(bytes).map(|res| RawResponse::Ok(res)) {
            Ok(res) => res,
            Err(_) => serde_json::from_slice(bytes)
                .map(|res| RawResponse::Error(res))
                .map_err(|e| ResponseError::Deserialize { error: e, bytes: bytes.to_owned() })?
        };

        let version = match &res {
            RawResponse::Ok(r) => &*r.jsonrpc,
            RawResponse::Error(e) => &*e.jsonrpc,
        };

        if version != "2.0" {
            return Err(ResponseError::InvalidVersion { bytes: bytes.to_owned() })
        }

        Ok(res)
    }

    /// Take ownership of a [`Response`], removing any lifetimes.
    pub fn into_owned(self) -> RawResponse<'static> {
        match self {
            RawResponse::Ok(res) => {
                RawResponse::Ok(res.into_owned())
            },
            RawResponse::Error(err) => {
                RawResponse::Error(err.into_owned())
            }
        }
    }

    /// Construct an "ok" response from an optional ID and serializable "result" value.
    /// Panics if the value given does not serialize to valid JSON.
    pub fn ok_from_value<V: serde::Serialize, Id: ToString>(id: Option<Id>, value: V) -> RawResponse<'static> {
        let value = serde_json::value::to_raw_value(&value).expect("invalid json");
        RawResponse::Ok(OkResponse {
            jsonrpc: "2.0".into(),
            id: id.map(|id| Cow::Owned(id.to_string())),
            result: Cow::Owned(value)
        })
    }

    /// Construct an "error" response from an optional ID and error object.
    pub fn err_from_value<Id: ToString>(id: Option<Id>, error: ErrorObject<'_>) -> RawResponse<'static> {
        RawResponse::Error(ErrorResponse {
            jsonrpc: "2.0".into(),
            id: id.map(|id| Cow::Owned(id.to_string())),
            error: error.into_owned()
        })
    }
}

/// A JSON-RPC "result" response.
#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct OkResponse<'a> {
    /// Always expected to be "2.0".
    #[serde(borrow)]
    pub jsonrpc: Cow<'a, str>,
    /// The message ID. None if this is a server notification.
    #[serde(borrow)]
    pub id: Option<Cow<'a, str>>,
    /// The result body.
    #[serde(borrow)]
    pub result: Cow<'a, RawValue>
}

impl <'a> OkResponse<'a> {
    /// Take ownership of a [`OkResponse`], removing any lifetimes.
    pub fn into_owned(self) -> OkResponse<'static> {
        OkResponse {
            jsonrpc: Cow::Owned(self.jsonrpc.into_owned()),
            id: self.id.map(|id| Cow::Owned(id.into_owned())),
            result: Cow::Owned(self.result.into_owned())
        }
    }
}

/// A JSON-RPC "error" response.
#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct ErrorResponse<'a> {
    /// Always expected to be "2.0".
    #[serde(borrow)]
    pub jsonrpc: Cow<'a, str>,
    /// The message ID. None if this is a server notification.
    #[serde(borrow)]
    pub id: Option<Cow<'a, str>>,
    /// Error details.
    #[serde(borrow)]
    pub error: ErrorObject<'a>
}

impl <'a> ErrorResponse<'a> {
    /// Take ownership of a [`ErrorResponse`], removing any lifetimes.
    pub fn into_owned(self) -> ErrorResponse<'static> {
        ErrorResponse {
            jsonrpc: Cow::Owned(self.jsonrpc.into_owned()),
            id: self.id.map(|id| Cow::Owned(id.into_owned())),
            error: self.error.into_owned()
        }
    }
}

/// Details about the JSON-RPC error response.
#[derive(serde::Deserialize, serde::Serialize, derive_more::Display, Clone, Debug)]
#[display(fmt = "Error {code}: {message}")]
pub struct ErrorObject<'a> {
    /// An error code.
    pub code: ErrorCode,
    /// A "pretty" error message.
    #[serde(borrow)]
    pub message: Cow<'a, str>,
    /// Some other optional error context.
    #[serde(borrow)]
    pub data: Option<Cow<'a, RawValue>>
}

impl <'a> ErrorObject<'a> {
    /// Take ownership of a [`ErrorObject`], removing any lifetimes.
    pub fn into_owned(self) -> ErrorObject<'static> {
        ErrorObject {
            code: self.code,
            message: Cow::Owned(self.message.into_owned()),
            data: self.data.map(|data| Cow::Owned(data.into_owned()))
        }
    }
}

/// An error code.
pub type ErrorCode = i32;
/// Invalid JSON was received by the server.
pub const CODE_PARSE_ERROR: ErrorCode = -32700;
/// The JSON send was not a valid request object.
pub const CODE_INVALID_REQUEST: ErrorCode = -32600;
/// THe method does not exist/is not available.
pub const CODE_METHOD_NOT_FOUND: ErrorCode = -32601;
/// Invalid method parameters.
pub const CODE_INVALID_PARAMS: ErrorCode = -32602;
/// Internal server error.
pub const CODE_INTERNAL_ERROR: ErrorCode = -32603;