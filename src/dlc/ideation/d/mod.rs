pub mod readable;
//pub mod readable_new;
pub mod byte_source_trait;
mod byte_state;
pub mod errors;
pub mod readable_sampling_b;
pub mod transform;
pub mod writable;
pub mod writable_new;
pub mod writable_send_and_non_send;

// Re-exports for compatibility with futures ecosystem
pub use futures_core::Stream;
pub use futures_sink::Sink;
pub use futures_sink::Sink as FuturesSink;

pub use readable::{ReadableSource, ReadableStream, ReadableStreamDefaultReader};
pub use transform::{TransformStream, Transformer};
pub use writable::{WritableSink, WritableStream, WritableStreamDefaultWriter};

use errors::StreamError;

/// Result of a stream operation
pub type StreamResult<T> = Result<T, StreamError>;

/// Status of a stream
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Readable,
    Closed,
    Errored,
}

/// Type-level marker for unlocked streams
pub struct Unlocked;
/// Type-level marker for locked streams  
pub struct Locked;

/// Generic queuing strategy trait
pub trait QueuingStrategy<T> {
    /// Return the size of the chunk
    fn size(&self, chunk: &T) -> usize;
    /// Return high water mark (desired max queue size)
    fn high_water_mark(&self) -> usize;
}

/// Count-based strategy
pub struct CountQueuingStrategy {
    high_water_mark: usize,
}

impl CountQueuingStrategy {
    pub const fn new(high_water_mark: usize) -> Self {
        Self { high_water_mark }
    }
}

impl<T> QueuingStrategy<T> for CountQueuingStrategy {
    fn size(&self, _chunk: &T) -> usize {
        1
    }

    fn high_water_mark(&self) -> usize {
        self.high_water_mark
    }
}

/// Byte length strategy for types with known byte sizes
pub struct ByteLengthQueuingStrategy {
    high_water_mark: usize,
}

impl ByteLengthQueuingStrategy {
    pub const fn new(high_water_mark: usize) -> Self {
        Self { high_water_mark }
    }
}

impl QueuingStrategy<Vec<u8>> for ByteLengthQueuingStrategy {
    fn size(&self, chunk: &Vec<u8>) -> usize {
        chunk.len()
    }

    fn high_water_mark(&self) -> usize {
        self.high_water_mark
    }
}

impl QueuingStrategy<String> for ByteLengthQueuingStrategy {
    fn size(&self, chunk: &String) -> usize {
        chunk.len()
    }

    fn high_water_mark(&self) -> usize {
        self.high_water_mark
    }
}

impl QueuingStrategy<&[u8]> for ByteLengthQueuingStrategy {
    fn size(&self, chunk: &&[u8]) -> usize {
        chunk.len()
    }

    fn high_water_mark(&self) -> usize {
        self.high_water_mark
    }
}
