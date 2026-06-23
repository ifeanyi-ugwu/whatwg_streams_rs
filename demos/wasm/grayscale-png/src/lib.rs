use png::Decoder;
use wasm_bindgen::prelude::*;
use wasm_bindgen::Clamped;
use wasm_bindgen_futures::spawn_local;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};
use whatwg_streams::{
    ReadableStream, StreamResult, TransformStream, TransformStreamDefaultController, Transformer,
};

/// Transform that grayscales RGBA pixels in place.
struct Grayscale;

impl Transformer<Vec<u8>, Vec<u8>> for Grayscale {
    async fn transform(
        &mut self,
        mut chunk: Vec<u8>,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        for px in chunk.chunks_mut(4) {
            let gray = ((px[0] as u16 + px[1] as u16 + px[2] as u16) / 3) as u8;
            px[0] = gray;
            px[1] = gray;
            px[2] = gray;
        }
        controller.enqueue(chunk)
    }
}

#[wasm_bindgen]
pub fn grayscale_png_stream_wasm(canvas_id: &str, png_bytes: Vec<u8>) {
    let canvas: HtmlCanvasElement = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(canvas_id)
        .unwrap()
        .dyn_into()
        .unwrap();
    let ctx: CanvasRenderingContext2d = canvas
        .get_context("2d")
        .unwrap()
        .unwrap()
        .dyn_into()
        .unwrap();

    spawn_local(async move {
        // Decode the PNG to RGBA pixels in Rust.
        let decoder = Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        let pixels = buf[..info.buffer_size()].to_vec();

        // Stream the pixels through the grayscale transform and draw the result.
        let source = ReadableStream::from_vec(vec![pixels]).spawn(spawn_local);
        let transform = TransformStream::builder(Grayscale).spawn(spawn_local);
        let output = source.pipe_through(transform, None).spawn(spawn_local);
        let (_lock, reader) = output.get_reader().unwrap();

        while let Ok(Some(chunk)) = reader.read().await {
            let clamped = Clamped(chunk.as_slice());
            let img =
                ImageData::new_with_u8_clamped_array_and_sh(clamped, info.width, info.height)
                    .unwrap();
            ctx.put_image_data(&img, 0.0, 0.0).unwrap();
        }
    });
}
