use wasm_bindgen::prelude::*;
use wasm_bindgen::Clamped;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};
use whatwg_streams::{
    ReadableStream, StreamResult, TransformStream, TransformStreamDefaultController, Transformer,
};

/// Transform that inverts RGBA pixel colors in place (alpha left untouched).
struct InvertColors;

impl Transformer<Vec<u8>, Vec<u8>> for InvertColors {
    async fn transform(
        &mut self,
        mut chunk: Vec<u8>,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        for px in chunk.chunks_mut(4) {
            px[0] = 255 - px[0];
            px[1] = 255 - px[1];
            px[2] = 255 - px[2];
        }
        controller.enqueue(chunk)
    }
}

#[wasm_bindgen]
pub async fn run_invert_demo(canvas_id: &str) {
    let canvas: HtmlCanvasElement = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(canvas_id)
        .unwrap()
        .dyn_into()
        .unwrap();
    let ctx: CanvasRenderingContext2d =
        canvas.get_context("2d").unwrap().unwrap().dyn_into().unwrap();

    let image_data = ctx
        .get_image_data(0.0, 0.0, canvas.width().into(), canvas.height().into())
        .unwrap();
    let pixels: Vec<u8> = image_data.data().0;

    // Stream the pixel buffer through the invert transform and draw each result back.
    let source = ReadableStream::from_vec(vec![pixels]).spawn(spawn_local);
    let transform = TransformStream::builder(InvertColors).spawn(spawn_local);
    let output = source.pipe_through(transform, None).spawn(spawn_local);
    let (_lock, reader) = output.get_reader().unwrap();

    while let Ok(Some(chunk)) = reader.read().await {
        let clamped = Clamped(&chunk[..]);
        let img =
            ImageData::new_with_u8_clamped_array_and_sh(clamped, canvas.width(), canvas.height())
                .unwrap();
        ctx.put_image_data(&img, 0.0, 0.0).unwrap();
    }
}
