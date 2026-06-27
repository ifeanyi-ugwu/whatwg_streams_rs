use super::{readable::ReadableByteStreamController, shared::StreamResult};
use crate::platform::MaybeSend;

pub trait ReadableByteSource: MaybeSend + 'static {
    fn start(
        &mut self,
        #[allow(unused)] controller: &mut ReadableByteStreamController,
    ) -> impl Future<Output = StreamResult<()>> + MaybeSend {
        async { Ok(()) }
    }

    /// Produce bytes into the stream.
    ///
    /// Hand chunks to the controller with `controller.enqueue(impl Into<Bytes>)`:
    /// a chunk already held as `Bytes` transfers in without a copy. A source
    /// reading from elsewhere owns its read buffer (e.g. fill a `BytesMut`, then
    /// `enqueue(buf.freeze())`) so the bytes reach the queue without being copied
    /// through the controller. Signal end-of-stream with `controller.close()`.
    fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
    ) -> impl Future<Output = StreamResult<()>> + MaybeSend;

    fn cancel(
        &mut self,
        #[allow(unused)] reason: Option<String>,
    ) -> impl Future<Output = StreamResult<()>> + MaybeSend {
        async { Ok(()) }
    }
}
