//! Identity tiles: the rounded monogram that stands in for a project or a
//! person wherever there is no uploaded image.
//!
//! The color is derived from the row's own id, not assigned or stored, so the
//! same project is the same color on every device and after every reinstall
//! without a byte of sync. Lightness and saturation come from the resolved
//! theme, so a tile reads at the same weight in light and dark and never
//! fights an imported palette.

use gpui::{Div, Hsla, SharedString, div, hsla, prelude::*, px};

use crate::theme::Theme;

/// Distinct hues on the wheel. A prime count avoids landing repeatedly on the
/// same few slots for sequentially-numbered ids, and 17 is far enough apart
/// that neighbors in a sidebar stay tellable at 15px.
const HUES: u64 = 17;

/// Stable hue in `0.0..1.0` for `seed`. FNV-1a: no allocation, no dependency,
/// and identical across platforms and releases — which is the whole point,
/// since the color is never written down anywhere.
pub fn hue(seed: &str) -> f32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    (h % HUES) as f32 / HUES as f32
}

/// The letter a tile shows: the first character of the label, uppercased.
/// Non-alphanumeric leads (a dotfile directory, an emoji-prefixed name) fall
/// through to the first character that carries meaning, then to `·` so the
/// tile is never blank.
pub fn monogram(label: &str) -> SharedString {
    label
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string().into())
        .unwrap_or_else(|| SharedString::from("·"))
}

/// A `size`-square rounded-rect tile for a *place* — a project, a folder.
///
/// Callers style nothing: the mark is self-contained so every surface that
/// shows one stays identical without copying six lines of styling around.
pub fn tile(seed: &str, label: &str, size: f32, theme: &Theme) -> Div {
    mark(seed, label, size, (size * 0.3).max(4.0), theme)
}

/// The circular variant, for a *person* — an account, a device owner. Same
/// derivation, so one identity reads the same wherever it appears.
pub fn avatar(seed: &str, label: &str, size: f32, theme: &Theme) -> Div {
    mark(seed, label, size, size / 2.0, theme)
}

fn mark(seed: &str, label: &str, size: f32, radius: f32, theme: &Theme) -> Div {
    let hue = hue(seed);
    let (fill, text) = fill_and_text(hue, theme);
    div()
        .size(px(size))
        .flex_none()
        .rounded(px(radius))
        .bg(fill)
        .flex()
        .items_center()
        .justify_center()
        // Tracks the tile rather than the type scale: a monogram is a mark,
        // and at 15px a body-sized glyph overflows its own corner radius.
        .text_size(px((size * 0.46).max(9.0)))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(text)
        .child(monogram(label))
}

/// Tile fill and its text color. Dark themes get a dim, desaturated wash with
/// a bright glyph; light themes invert that, so contrast holds either way
/// without measuring the specific palette.
fn fill_and_text(hue: f32, theme: &Theme) -> (Hsla, Hsla) {
    if theme.appearance.is_light() {
        (hsla(hue, 0.52, 0.86, 1.0), hsla(hue, 0.72, 0.28, 1.0))
    } else {
        (hsla(hue, 0.42, 0.26, 1.0), hsla(hue, 0.78, 0.78, 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hue_is_stable_and_bounded() {
        // The contract the whole module rests on: never stored, so it must be
        // reproducible byte-for-byte, on any platform, forever.
        assert_eq!(hue("space-7"), hue("space-7"));
        assert_ne!(hue("space-7"), hue("space-8"));
        for seed in ["", "a", "space-7", "/Users/x/code/very-long-project-name"] {
            let h = hue(seed);
            assert!((0.0..1.0).contains(&h), "{seed} produced {h}");
        }
    }

    #[test]
    fn monogram_skips_punctuation_and_never_blanks() {
        assert_eq!(monogram("zeron"), SharedString::from("Z"));
        assert_eq!(monogram(".dotfiles"), SharedString::from("D"));
        assert_eq!(monogram("  spaced"), SharedString::from("S"));
        assert_eq!(monogram("42-tests"), SharedString::from("4"));
        assert_eq!(monogram(""), SharedString::from("·"));
        assert_eq!(monogram("—"), SharedString::from("·"));
    }
}
