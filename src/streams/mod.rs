pub mod byte_source_trait;
pub mod byte_state;
pub mod error;
pub mod readable;
pub mod transform;
pub mod writable;

// Re-export main types
pub use byte_source_trait::*;
pub use byte_state::*;
pub use error::*;
pub use readable::*;
pub use transform::*;
pub use writable::*;

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
#[derive(Clone)]
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
#[derive(Clone)]
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
