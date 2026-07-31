use std::fmt;

/// Errors produced by the QRFerry codec.
#[derive(Debug)]
pub enum Error {
    /// Input exceeds the 512 MiB browser-build limit.
    FileTooLarge { size: usize, max: usize },
    /// A symbol size that cannot be encoded on the optical channel.
    InvalidSymbolSize(u16),
    /// A QF4 frame failed validation.
    InvalidFrame(&'static str),
    /// A QFC4 container failed validation.
    InvalidContainer(&'static str),
    /// A RaptorQ transport packet failed validation.
    InvalidPacket(&'static str),
    /// The RaptorQ layer rejected the operation.
    RaptorQ(&'static str),
    /// QR encoding failed.
    QrEncode(String),
    /// Unknown transfer preset key.
    UnknownPreset(String),
    /// I/O failure (compression/decompression).
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::FileTooLarge { size, max } => write!(
                f,
                "This browser build supports files up to {} bytes, got {}",
                max, size
            ),
            Error::InvalidSymbolSize(size) => {
                write!(f, "The optical symbol size is invalid: {}", size)
            }
            Error::InvalidFrame(message) => write!(f, "{}", message),
            Error::InvalidContainer(message) => write!(f, "{}", message),
            Error::InvalidPacket(message) => write!(f, "{}", message),
            Error::RaptorQ(message) => write!(f, "{}", message),
            Error::QrEncode(message) => write!(f, "QR encoding failed: {}", message),
            Error::UnknownPreset(key) => {
                write!(f, "unknown transfer preset: {}", key)
            }
            Error::Io(cause) => write!(f, "{}", cause),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(cause) => Some(cause),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(cause: std::io::Error) -> Error {
        Error::Io(cause)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
