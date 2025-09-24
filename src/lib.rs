pub mod streams;
pub use send::{
    readable::{
        ReadableByteSource, ReadableSource, ReadableStream, ReadableStreamDefaultController,
    },
    transform::{
        IdentityTransformer, TransformStream, TransformStreamDefaultController, Transformer,
    },
    writable::{WritableSink, WritableStream, WritableStreamDefaultController},
};
pub use streams::{send::*, *};

pub fn add(left: u64, right: u64) -> u64 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
