use serde_json::value::RawValue;
use std::borrow::Cow;
use std::sync::Arc;

#[derive(derive_more::From, derive_more::Display, Clone)]
pub enum ResponseError {
    #[from]
    #[display(fmt = "{_0}")]
    Deserialize(Arc<serde_json::Error>),
    #[display(fmt = "failed to decode response: expected '\"jsonrpc\": \"2.0\"'")]
    InvalidVersion
}

/// A JSON-RPC response is either a "result" or "error" payload.
/// This represents the shape a message can deserialize into.
#[derive(Clone, Debug)]
pub enum Response<'a> {
    Ok(OkResponse<'a>),
    Err(ErrorResponse<'a>)
}

impl <'a> Response<'a> {
    /// Decode some bytes into a valid JSON-RPC response or
    /// return an error if it's not valid.
    pub fn from_bytes(bytes: &[u8]) -> Result<Response<'_>, ResponseError> {
        let res = match serde_json::from_slice(bytes).map(|res| Response::Ok(res)) {
            Ok(res) => res,
            Err(_) => serde_json::from_slice(bytes)
                .map(|res| Response::Err(res))
                .map_err(|e| Arc::new(e))?
        };

        let version = match &res {
            Response::Ok(r) => &*r.jsonrpc,
            Response::Err(e) => &*e.jsonrpc,
        };

        if version != "2.0" {
            return Err(ResponseError::InvalidVersion)
        }

        Ok(res)
    }

    /// Take ownership of a [`Response`], removing any lifetimes.
    pub fn into_owned(self) -> Response<'static> {
        match self {
            Response::Ok(res) => {
                Response::Ok(res.into_owned())
            },
            Response::Err(err) => {
                Response::Err(err.into_owned())
            }
        }
    }
}

#[derive(serde::Deserialize, Clone, Debug)]
pub struct OkResponse<'a> {
    #[serde(borrow)]
    jsonrpc: Cow<'a, str>,
    #[serde(borrow)]
    pub id: Option<Cow<'a, str>>,
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

#[derive(serde::Deserialize, Clone, Debug)]
pub struct ErrorResponse<'a> {
    #[serde(borrow)]
    jsonrpc: Cow<'a, str>,
    #[serde(borrow)]
    pub id: Option<Cow<'a, str>>,
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

#[derive(serde::Deserialize, derive_more::Display, Clone, Debug)]
#[display(fmt = "Error {code}: {message}")]
pub struct ErrorObject<'a> {
    pub code: i32,
    #[serde(borrow)]
    pub message: Cow<'a, str>,
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