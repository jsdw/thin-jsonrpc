use serde_json::value::RawValue;
use serde::Serialize;

/// A trait which is implemented for anything which can be turned into
/// valid RPC parameters.
pub trait IntoRpcParams {
    /// Return the params. These are expected to be encoded to JSON,
    /// and take the form of either an array or object of parameters.
    /// Can return `None` to avoid allocation when returning no parameters.
    fn into_rpc_params(self) -> RpcParams;
}

/// The RPC params we'll return from our implementations of [`IntoRpcParams`].
pub type RpcParams = Option<Box<RawValue>>;

impl IntoRpcParams for RpcParams {
    fn into_rpc_params(self) -> RpcParams {
        self
    }
}

/// Parameter builder to build valid "object or "named" parameters.
/// This is the equivalent of a JSON Map object `{ key: value }`.
///
/// # Examples
///
/// ```rust
///
/// use jsonrpc_client::params::ObjectParams;
///
/// let mut builder = ObjectParams::new();
/// builder.insert("param1", 1);
/// builder.insert("param2", "abc");
///
/// // Use RPC parameters...
/// ```
#[derive(Clone, Debug)]
pub struct ObjectParams {
    bytes: Vec<u8>
}

impl ObjectParams {
    /// Construct a new [`ObjectParams`] instance.
    pub fn new() -> Self {
        ObjectParams { bytes: Vec::new() }
    }

    /// Insert a new named parameter.
    pub fn insert<P: Serialize>(mut self, name: &str, value: P) -> Self {
        if self.bytes.is_empty() {
            self.bytes.push(b'{');
        } else {
            self.bytes.push(b',');
        }

        serde_json::to_writer(&mut self.bytes, name)
            .expect("should always be valid");
        self.bytes.push(b':');
        serde_json::to_writer(&mut self.bytes, &value)
            .expect("invalid JSON");

        self
    }
}

impl IntoRpcParams for ObjectParams {
    fn into_rpc_params(mut self) -> RpcParams {
        if self.bytes.is_empty() {
            return None;
        }

        self.bytes.push(b'}');
        // Safety: This is safe because JSON does not emit invalid UTF-8:
        let utf8_string = unsafe { String::from_utf8_unchecked(self.bytes) };
        Some(RawValue::from_string(utf8_string).expect("valid JSON expected"))
    }
}

/// Parameter builder to build valid "array" or "unnamed" JSON-RPC parameters.
/// This is the equivalent of a JSON array like `[ value0, value1, .., valueN ]`.
///
/// # Examples
///
/// ```rust
///
/// use jsonrpc_client::params::ArrayParams;
///
/// let mut builder = ArrayParams::new();
/// builder.insert("param1");
/// builder.insert(1);
///
/// // Use RPC parameters...
/// ```
#[derive(Clone, Debug)]
pub struct ArrayParams {
    bytes: Vec<u8>
}

impl ArrayParams {
    /// Construct a new [`ArrayParams`] instance.
    pub fn new() -> Self {
        ArrayParams { bytes: Vec::new() }
    }

    /// Insert a new parameter.
    pub fn insert<P: Serialize>(mut self, value: P) -> Self {
        if self.bytes.is_empty() {
            self.bytes.push(b'[');
        } else {
            self.bytes.push(b',');
        }

        serde_json::to_writer(&mut self.bytes, &value)
            .expect("invalid JSON");

        self
    }
}

impl IntoRpcParams for ArrayParams {
    fn into_rpc_params(mut self) -> RpcParams {
        if self.bytes.is_empty() {
            return None;
        }

        self.bytes.push(b']');
        // Safety: This is safe because JSON does not emit invalid UTF-8:
        let utf8_string = unsafe { String::from_utf8_unchecked(self.bytes) };
        Some(RawValue::from_string(utf8_string).expect("valid JSON expected"))
    }
}

/// Construct positional/array parameters for a JSON-RPC Call.
#[macro_export]
macro_rules! params {
    ($($param:expr),*) => {{
        let mut a = $crate::params::ArrayParams::new();
        $(
            a = a.insert($param);
        )*
        a
    }}
}

pub use params;