mod byte_source_trait;
mod byte_state;
pub mod error;
mod macros;
pub mod readable;
pub mod transform;
pub mod writable;

pub type StreamResult<T> = Result<T, error::StreamError>;
