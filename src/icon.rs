//! App icon rasterized from the repository's AppIcon.svg artwork.

/// Icon edge length in pixels.
pub const SIZE: u32 = 64;

/// Rasterize the canonical SVG into RGBA pixels for the native window icon.
pub fn rgba() -> Vec<u8> {
    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(include_bytes!("../assets/AppIcon.svg"), &options)
        .expect("AppIcon.svg must be a valid SVG");
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(SIZE, SIZE).expect("64x64 icon pixmap allocation");
    let scale = SIZE as f32 / tree.size().width().max(tree.size().height());
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    pixmap.data().to_vec()
}

/// Encode the SVG-derived RGBA pixels as a Windows .ico resource.
pub fn ico_bytes() -> Vec<u8> {
    let w = SIZE as usize;
    let h = SIZE as usize;
    let rgba = rgba();
    let and_stride = w.div_ceil(32) * 4;
    let dib_size = 40 + w * h * 4 + and_stride * h;
    let mut ico = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.push(w as u8);
    ico.push(h as u8);
    ico.push(0);
    ico.push(0);
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&32u16.to_le_bytes());
    ico.extend_from_slice(&(dib_size as u32).to_le_bytes());
    ico.extend_from_slice(&22u32.to_le_bytes());
    ico.extend_from_slice(&40u32.to_le_bytes());
    ico.extend_from_slice(&(w as i32).to_le_bytes());
    ico.extend_from_slice(&((h * 2) as i32).to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&32u16.to_le_bytes());
    ico.extend_from_slice(&0u32.to_le_bytes());
    ico.extend_from_slice(&0u32.to_le_bytes());
    ico.extend_from_slice(&0i32.to_le_bytes());
    ico.extend_from_slice(&0i32.to_le_bytes());
    ico.extend_from_slice(&0u32.to_le_bytes());
    ico.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h).rev() {
        for x in 0..w {
            let i = (y * w + x) * 4;
            ico.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    ico.extend(std::iter::repeat_n(0u8, h * and_stride));
    ico
}
