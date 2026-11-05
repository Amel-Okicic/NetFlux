//! Build script: embed the elevation manifest and the AppIcon.svg artwork.

fn main() {
    #[cfg(target_os = "windows")]
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        use embed_manifest::manifest::ExecutionLevel;
        use embed_manifest::{embed_manifest, new_manifest};
        embed_manifest(
            new_manifest("NetFlux.App")
                .requested_execution_level(ExecutionLevel::RequireAdministrator),
        )
        .expect("failed to embed requireAdministrator manifest");
    }

    #[cfg(target_os = "windows")]
    {
        let options = resvg::usvg::Options::default();
        let tree = resvg::usvg::Tree::from_data(include_bytes!("assets/AppIcon.svg"), &options)
            .expect("assets/AppIcon.svg must be valid");
        let size = 256u32;
        let mut pixmap =
            resvg::tiny_skia::Pixmap::new(size, size).expect("app icon pixmap allocation");
        let scale = size as f32 / tree.size().width().max(tree.size().height());
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        let rgba = pixmap.data();
        let w = size as usize;
        let h = size as usize;
        let and_stride = w.div_ceil(32) * 4;
        let dib_size = 40 + w * h * 4 + and_stride * h;
        let mut ico = Vec::new();
        ico.extend_from_slice(&0u16.to_le_bytes());
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&[0, 0, 0, 0]);
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
        let icon_path =
            std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("netflux.ico");
        std::fs::write(&icon_path, ico).expect("write generated Windows icon");
        let mut res = winresource::WindowsResource::new();
        res.set_icon(icon_path.to_str().expect("icon path UTF-8"));
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon embed skipped: {e}");
        }
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=netflux.exe.manifest");
    println!("cargo:rerun-if-changed=assets/AppIcon.svg");
    println!("cargo:rerun-if-changed=assets/DownloadIco.svg");
    println!("cargo:rerun-if-changed=assets/UploadIco.svg");
}
