use js_sys::{Function, Object, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlElement;
use whatwg_streams::{
    ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamResult, TransformStream,
    TransformStreamDefaultController, Transformer, WritableSink, WritableStream,
    WritableStreamDefaultController,
};

/// Wraps a fetch response body's reader as a byte `ReadableSource`.
struct FetchByteSource {
    reader: Object,
}

impl ReadableSource<Vec<u8>> for FetchByteSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        let read_fn: Function = Reflect::get(&self.reader, &JsValue::from_str("read"))
            .unwrap()
            .dyn_into()
            .unwrap();
        let promise: js_sys::Promise = read_fn.call0(&self.reader).unwrap().dyn_into().unwrap();
        let result = wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();

        let done = Reflect::get(&result, &JsValue::from_str("done"))
            .unwrap()
            .as_bool()
            .unwrap_or(false);
        if done {
            controller.close()?;
        } else {
            let value = Reflect::get(&result, &JsValue::from_str("value")).unwrap();
            controller.enqueue(Uint8Array::new(&value).to_vec())?;
        }
        Ok(())
    }
}

/// Parses PNG container structure from a byte stream, emitting one line per chunk.
///
/// A PNG is an 8-byte signature followed by chunks laid out as
/// `length(4) · type(4) · data(length) · crc(4)`. Chunks can straddle network reads, so
/// bytes are buffered until a whole chunk is present.
#[derive(Default)]
struct PngChunkParser {
    buf: Vec<u8>,
    signature_seen: bool,
}

impl Transformer<Vec<u8>, String> for PngChunkParser {
    async fn transform(
        &mut self,
        chunk: Vec<u8>,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> StreamResult<()> {
        self.buf.extend_from_slice(&chunk);

        if !self.signature_seen {
            if self.buf.len() < 8 {
                return Ok(());
            }
            self.buf.drain(..8); // PNG signature
            self.signature_seen = true;
        }

        // Need length(4) + type(4) to read a header, then 4 more bytes of CRC after data.
        while self.buf.len() >= 8 {
            let length = u32::from_be_bytes(self.buf[0..4].try_into().unwrap()) as usize;
            if self.buf.len() < 12 + length {
                break; // wait for the rest of this chunk
            }
            let kind = String::from_utf8_lossy(&self.buf[4..8]).into_owned();
            controller.enqueue(format!("{kind}  ({length} bytes)"))?;
            self.buf.drain(..12 + length);
        }
        Ok(())
    }
}

/// Appends each parsed chunk description to a parent element as a `<div>`.
struct ChunkListSink {
    parent: HtmlElement,
}

impl WritableSink<String> for ChunkListSink {
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
pub fn run_png_chunks_demo(url: String, output_id: String) {
    spawn_local(async move {
        let document = web_sys::window().unwrap().document().unwrap();
        let parent: HtmlElement = document
            .get_element_by_id(&output_id)
            .unwrap()
            .dyn_into()
            .unwrap();

        let response = match wasm_bindgen_futures::JsFuture::from(
            web_sys::window().unwrap().fetch_with_str(&url),
        )
        .await
        {
            Ok(r) => r.dyn_into::<web_sys::Response>().unwrap(),
            Err(_) => {
                parent.set_text_content(Some("Fetch failed (network or CORS)."));
                return;
            }
        };
        let reader = response.body().unwrap().get_reader();

        let source = ReadableStream::builder(FetchByteSource { reader }).spawn(spawn_local);
        let parser = TransformStream::builder(PngChunkParser::default()).spawn(spawn_local);
        let sink = WritableStream::builder(ChunkListSink {
            parent: parent.clone(),
        })
        .spawn(spawn_local);

        let chunks = source.pipe_through(parser, None).spawn(spawn_local);
        chunks.pipe_to(&sink, None).await.unwrap();

        // Completion marker. If this never appears, the stream isn't reaching its end.
        let done = document.create_element("div").unwrap();
        done.set_text_content(Some("— end of PNG —"));
        let _ = parent.append_child(&done);
    });
}
