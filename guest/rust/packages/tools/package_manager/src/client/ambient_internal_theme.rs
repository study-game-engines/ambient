use ambient_api::{
    core::{
        rendering::components::color,
        text::components::{font_family, font_size},
    },
    prelude::{cb, vec4, Element, WindowStyle},
    ui::UIExt,
};
use ambient_color::Color;
use ambient_design_tokens::LIGHT::{
    SEMANTIC_MAIN_ELEMENTS_PRIMARY, SEMANTIC_MAIN_SURFACE_PRIMARY, SEMANTIC_MAIN_SURFACE_SECONDARY,
};
pub fn window_style() -> WindowStyle {
    WindowStyle {
        body: cb(|e| e.hex_background(SEMANTIC_MAIN_SURFACE_SECONDARY)),
        title_bar: cb(|e| e.hex_background(SEMANTIC_MAIN_SURFACE_PRIMARY)),
        title_text: cb(|e| {
            e.mono_xs_500upp()
                .hex_text_color(SEMANTIC_MAIN_ELEMENTS_PRIMARY)
        }),
    }
}

pub trait AmbientInternalStyle {
    fn hex_background(self, hex: &str) -> Self;
    fn hex_text_color(self, hex: &str) -> Self;
    fn mono(self) -> Self;
    fn mono_xs_500upp(self) -> Self;
    fn mono_s_500upp(self) -> Self;
}
impl AmbientInternalStyle for Element {
    fn hex_background(self, hex: &str) -> Self {
        self.with_background(Color::hex(hex).unwrap().into())
    }
    fn hex_text_color(self, hex: &str) -> Self {
        self.with(color(), Color::hex(hex).unwrap().into())
    }
    fn mono(self) -> Self {
        self.with(font_family(), "https://internal-content.fra1.cdn.digitaloceanspaces.com/fonts/DiatypeMono/ABCDiatypeMono-Medium.otf".to_string())
    }
    fn mono_xs_500upp(self) -> Self {
        self.mono().with(font_size(), 12.8)
    }
    fn mono_s_500upp(self) -> Self {
        self.mono().with(font_size(), 16.)
    }
}

pub const SEMANTIC_MAIN_ELEMENTS_TERTIARY: &str = "#595959";
