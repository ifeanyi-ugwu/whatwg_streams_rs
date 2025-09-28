mod byte_source_trait;
mod byte_state;
pub mod error;
mod macros;
pub mod readable;
pub mod transform;
pub mod writable;

pub type StreamResult<T> = Result<T, error::StreamError>;

pub use readable::{
    ReadableByteSource, ReadableSource, ReadableStream, ReadableStreamDefaultController,
};
pub use transform::{
    IdentityTransformer, TransformStream, TransformStreamDefaultController, Transformer,
};
pub use writable::{
    WritableSink, WritableStream, WritableStreamDefaultController, WritableStreamDefaultWriter,
};
