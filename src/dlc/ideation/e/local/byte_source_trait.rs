use super::{super::StreamResult, readable::ReadableByteStreamController};

pub trait ReadableByteSource: Sized + 'static {
    fn start(
        &mut self,
        controller: &mut ReadableByteStreamController,
    ) -> impl Future<Output = StreamResult<()>> {
        async { Ok(()) }
    }

    fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
        buffer: &mut [u8],
    ) -> impl Future<Output = StreamResult<usize>>;

    fn cancel(&mut self, reason: Option<String>) -> impl Future<Output = StreamResult<()>> {
        async { Ok(()) }
    }
}
