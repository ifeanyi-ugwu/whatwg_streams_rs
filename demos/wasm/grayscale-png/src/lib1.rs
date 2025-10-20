use js_sys::Uint8ClampedArray;
use wasm_bindgen::prelude::*;
use wasm_bindgen::Clamped;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};
use whatwg_streams::local::{ReadableStream, TransformStream, Transformer};

// Transformer: Grayscale
struct Grayscale;

impl Transformer<Vec<u8>, Vec<u8>> for Grayscale {
    async fn transform(
        &mut self,
        mut chunk: Vec<u8>,
        controller: &mut whatwg_streams::local::TransformStreamDefaultController<Vec<u8>>,
    ) -> Result<(), whatwg_streams::local::error::StreamError> {
        for px in chunk.chunks_mut(4) {
            let gray = ((px[0] as u16 + px[1] as u16 + px[2] as u16) / 3) as u8;
            px[0] = gray;
            px[1] = gray;
            px[2] = gray;
        }
        controller.enqueue(chunk);
        Ok(())
    }
}

#[wasm_bindgen]
pub fn run_grayscale_canvas(canvas_id: &str) {
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

    // Grab pixels
    let image_data = ctx
        .get_image_data(0.0, 0.0, canvas.width().into(), canvas.height().into())
        .unwrap();
    let pixels: Vec<u8> = Uint8ClampedArray::new(&image_data.data().into()).to_vec();

    spawn_local(async move {
        // Source stream
        let source = ReadableStream::from_vec(vec![pixels]).spawn(spawn_local);
        // Transform stream
        let transform = TransformStream::builder(Grayscale).spawn(spawn_local);
        // Pipe through transform
        let output = source.pipe_through(transform, None).spawn(spawn_local);
        let (_, reader) = output.get_reader().unwrap();

        while let Ok(Some(chunk)) = reader.read().await {
            let clamped = Clamped(chunk.as_slice());
            let new_image_data = ImageData::new_with_u8_clamped_array_and_sh(
                clamped,
                canvas.width(),
                canvas.height(),
            )
            .unwrap();
            ctx.put_image_data(&new_image_data, 0.0, 0.0).unwrap();
        }
    });
}
