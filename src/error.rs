use crate::{
    serde::{DeserializeError, SerializeError},
    version::Version,
    CompressionType,
};

/// Represents errors that can occur in the LSM-tree
#[derive(Debug)]
pub enum Error {
    /// I/O error
    Io(std::io::Error),

    /// Serialization failed
    Serialize(SerializeError),

    /// Deserialization failed
    Deserialize(DeserializeError),

    /// Decompression failed
    Decompress(CompressionType),

    /// Invalid or unparseable data format version
    InvalidVersion(Option<Version>),

    /// Value log errors
    #[cfg(feature = "kv_sep")]
    ValueLog(value_log::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LsmTreeError: {self:?}")
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<SerializeError> for Error {
    fn from(value: SerializeError) -> Self {
        Self::Serialize(value)
    }
}

impl From<DeserializeError> for Error {
    fn from(value: DeserializeError) -> Self {
        Self::Deserialize(value)
    }
}

#[cfg(feature = "kv_sep")]
impl From<value_log::Error> for Error {
    fn from(value: value_log::Error) -> Self {
        Self::ValueLog(value)
    }
}

/// Tree result
pub type Result<T> = std::result::Result<T, Error>;
