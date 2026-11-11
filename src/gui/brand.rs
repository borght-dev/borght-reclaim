use eframe::egui::{self, TextureFilter, TextureOptions};

const LOGO_PNG: &[u8] = include_bytes!("../../assets/logo-256.png");
pub const HEADER_LOGO_POINTS: f32 = 28.0;

pub fn window_icon() -> egui::IconData {
    let pixels = logo_pixels();
    egui::IconData { width: pixels.width(), height: pixels.height(), rgba: pixels.into_raw() }
}

pub fn logo(context: &egui::Context) -> egui::TextureHandle {
    let pixels = logo_pixels();
    let size = [pixels.width() as usize, pixels.height() as usize];
    let image = egui::ColorImage::from_rgba_unmultiplied(size, pixels.as_raw());
    let shrunk_smoothly =
        TextureOptions { mipmap_mode: Some(TextureFilter::Linear), ..TextureOptions::LINEAR };
    context.load_texture("brand-logo", image, shrunk_smoothly)
}

fn logo_pixels() -> image::RgbaImage {
    image::load_from_memory_with_format(LOGO_PNG, image::ImageFormat::Png)
        .expect("the embedded logo is a valid PNG")
        .into_rgba8()
}
