use super::{readable::ReadableByteStreamController, shared::StreamResult};
use crate::platform::MaybeSend;

pub trait ReadableByteSource: MaybeSend + 'static {
    fn start(
        &mut self,
        #[allow(unused)] controller: &mut ReadableByteStreamController,
    ) -> impl Future<Output = StreamResult<()>> + MaybeSend {
        async { Ok(()) }
    }

    fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
        buffer: &mut [u8],
    ) -> impl Future<Output = StreamResult<usize>> + MaybeSend;

    fn cancel(
        &mut self,
        #[allow(unused)] reason: Option<String>,
    ) -> impl Future<Output = StreamResult<()>> + MaybeSend {
        async { Ok(()) }
    }
}
