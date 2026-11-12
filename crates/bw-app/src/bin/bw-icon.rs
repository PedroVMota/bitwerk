//! Renders the bitwerk app icon from code (the same symbol geometry as the
//! canvas, see `bw_render::symbols::raster::app_icon`) into PNG files:
//!
//! ```text
//! bw-icon <iconset-dir> <embedded-png>
//! ```
//!
//! writes the macOS `.iconset` sizes (16–1024 px) for `iconutil` and the
//! 256 px PNG embedded in the binary as the window icon. Run through
//! `make icon`; nothing is downloaded.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

fn save(path: &Path, size: u32) -> Result<(), Box<dyn std::error::Error>> {
    let rgba = bw_render::symbols::raster::app_icon(size);
    let img = image::RgbaImage::from_raw(size, size, rgba).ok_or("bad icon buffer")?;
    img.save(path)?;
    println!("wrote {} ({size}x{size})", path.display());
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(iconset), Some(embedded)) = (args.first(), args.get(1)) else {
        return Err("usage: bw-icon <iconset-dir> <embedded-png>".into());
    };
    let dir = PathBuf::from(iconset);
    std::fs::create_dir_all(&dir)?;
    for base in [16u32, 32, 128, 256, 512] {
        save(&dir.join(format!("icon_{base}x{base}.png")), base)?;
        save(&dir.join(format!("icon_{base}x{base}@2x.png")), base * 2)?;
    }
    save(Path::new(embedded), 256)?;
    Ok(())
}
