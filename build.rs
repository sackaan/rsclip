/// Embed a multi-resolution .ico (generated from src/assets/icon.png) into the
/// Windows executable so Explorer and the taskbar show the app icon.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }

    let png_path = std::path::Path::new("src/assets/icon.png");
    let ico_path = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("icon.ico");

    if png_path.exists() {
        let img = image::open(png_path).expect("Failed to load src/assets/icon.png");
        let sizes: &[u32] = &[16, 32, 48, 64, 128, 256];

        let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);
        for &size in sizes {
            let resized = img.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
            let rgba = resized.to_rgba8();
            let icon_image = ico::IconImage::from_rgba_data(size, size, rgba.into_raw());
            icon_dir.add_entry(ico::IconDirEntry::encode(&icon_image).expect("ICO encode failed"));
        }

        let mut file = std::fs::File::create(&ico_path).expect("Failed to create icon.ico");
        icon_dir.write(&mut file).expect("Failed to write icon.ico");
    }

    let mut res = winresource::WindowsResource::new();
    if ico_path.exists() {
        res.set_icon(ico_path.to_str().unwrap());
    }
    res.set("ProductName", "RSClip");
    res.set("FileDescription", "RSClip");
    res.compile().expect("Failed to compile Windows resources");
}
