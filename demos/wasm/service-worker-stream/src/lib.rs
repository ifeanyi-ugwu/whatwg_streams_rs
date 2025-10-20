use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::{Document, HtmlElement};
use whatwg_streams::local::{WritableSink, WritableStream, WritableStreamDefaultController};

// Sink for appending <div> chunks
struct StreamingElementSink {
    parent: HtmlElement,
}

impl WritableSink<String> for StreamingElementSink {
    fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> impl std::future::Future<Output = whatwg_streams::local::StreamResult<()>> {
        async move {
            let document: Document = web_sys::window().unwrap().document().unwrap();
            let div = document.create_element("div").unwrap();
            div.set_inner_html(&chunk);
            self.parent.append_child(&div).unwrap();
            Ok(())
        }
    }
}

#[wasm_bindgen]
pub struct StreamingElementHandle {
    writer: whatwg_streams::local::WritableStreamDefaultWriter<String, StreamingElementSink>,
}

#[wasm_bindgen]
impl StreamingElementHandle {
    #[wasm_bindgen(constructor)]
    pub fn new(element_id: &str) -> StreamingElementHandle {
        let document = web_sys::window().unwrap().document().unwrap();
        let el: HtmlElement = document
            .get_element_by_id(element_id)
            .unwrap()
            .dyn_into()
            .unwrap();
        let sink = StreamingElementSink { parent: el };
        let writable = WritableStream::builder(sink).spawn(spawn_local);
        let (_, writer) = writable.get_writer().unwrap();
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
