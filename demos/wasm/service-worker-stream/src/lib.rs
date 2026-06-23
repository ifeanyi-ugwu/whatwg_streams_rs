use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlElement;
use whatwg_streams::{
    StreamResult, WritableSink, WritableStream, WritableStreamDefaultController,
    WritableStreamDefaultWriter,
};

/// Sink that appends each streamed chunk into a parent element as a `<div>`.
struct StreamingElementSink {
    parent: HtmlElement,
}

impl WritableSink<String> for StreamingElementSink {
    async fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        let document = web_sys::window().unwrap().document().unwrap();
        let div = document.create_element("div").unwrap();
        div.set_text_content(Some(&chunk));
        self.parent.append_child(&div).unwrap();
        Ok(())
    }
}

/// JS-facing handle: construct it against an element, feed it chunks read from the
/// service-worker response, then close it.
#[wasm_bindgen]
pub struct StreamingElementHandle {
    writer: WritableStreamDefaultWriter<String, StreamingElementSink>,
}

#[wasm_bindgen]
impl StreamingElementHandle {
    #[wasm_bindgen(constructor)]
    pub fn new(element_id: &str) -> StreamingElementHandle {
        let el: HtmlElement = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .get_element_by_id(element_id)
            .unwrap()
            .dyn_into()
            .unwrap();
        let writable =
            WritableStream::builder(StreamingElementSink { parent: el }).spawn(spawn_local);
        let (_lock, writer) = writable.get_writer().unwrap();
        StreamingElementHandle { writer }
    }

    pub fn write_chunk(&mut self, chunk: String) {
        let mut writer = self.writer.clone();
        spawn_local(async move {
            writer.write(chunk).await.unwrap();
        });
    }

    pub fn close(&mut self) {
        let mut writer = self.writer.clone();
        spawn_local(async move {
            writer.close().await.unwrap();
        });
    }
}
