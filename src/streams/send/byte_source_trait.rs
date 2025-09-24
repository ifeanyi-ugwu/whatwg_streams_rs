use super::{super::StreamResult, readable::ReadableByteStreamController};

pub trait ReadableByteSource: Send + Sized + 'static {
    fn start(
        &mut self,
        controller: &mut ReadableByteStreamController,
    ) -> impl Future<Output = StreamResult<()>> + Send {
        async { Ok(()) }
    }

    fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
        buffer: &mut [u8],
    ) -> impl Future<Output = StreamResult<usize>> + Send;

    fn cancel(&mut self, reason: Option<String>) -> impl Future<Output = StreamResult<()>> + Send {
        async { Ok(()) }
    }
}
