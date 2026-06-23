use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlElement;
use whatwg_streams::{StreamResult, WritableSink, WritableStream, WritableStreamDefaultController};

/// Sink that appends each streamed chunk into the target element as a `<div>`.
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

#[wasm_bindgen]
pub fn run_streaming_element_demo(element_id: &str) {
    let el: HtmlElement = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(element_id)
        .unwrap()
        .dyn_into()
        .unwrap();

    let writable = WritableStream::builder(StreamingElementSink { parent: el }).spawn(spawn_local);

    spawn_local(async move {
        let (_lock, writer) = writable.get_writer().unwrap();
        for chunk in [
            "Hello from Rust + WASM!",
            "Streaming into a custom element.",
            "Each chunk is appended as a <div>.",
        ] {
            writer.write(chunk.to_string()).await.unwrap();
        }
        writer.close().await.unwrap();
    });
}
