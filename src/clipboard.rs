use anyhow::{bail, Context, Result};
use base64::Engine;
use std::path::Path;

use crate::session::ImageAttachment;

/// Largest image we will attach; providers reject bigger payloads anyway.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Reads an image from the system clipboard (Alt+V in the TUI).
/// Falls back to clipboard text that is a path to an image file,
/// which is what Explorer's "Copy as path" puts there.
pub fn image_from_clipboard() -> Result<ImageAttachment> {
    let mut cb = arboard::Clipboard::new().context("clipboard unavailable")?;
    if let Ok(img) = cb.get_image() {
        let png = encode_png(img.width as u32, img.height as u32, &img.bytes)?;
        return attachment("image/png", &png);
    }
    if let Ok(text) = cb.get_text() {
        if let Some(path) = image_path(&text) {
            return image_from_path(Path::new(&path));
        }
    }
    bail!("no image in clipboard")
}

/// Loads an image file from disk as an attachment.
pub fn image_from_path(path: &Path) -> Result<ImageAttachment> {
    let media_type = media_type_for(path)
        .with_context(|| format!("not a supported image: {}", path.display()))?;
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    attachment(media_type, &bytes)
}

/// If `text` (possibly quoted, as terminals paste dropped files) names an
/// existing image file, returns the unquoted path.
pub fn image_path(text: &str) -> Option<String> {
    let t = text.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    if t.is_empty() || t.contains('\n') {
        return None;
    }
    let p = Path::new(t);
    (media_type_for(p).is_some() && p.is_file()).then(|| t.to_string())
}

fn media_type_for(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

fn attachment(media_type: &str, bytes: &[u8]) -> Result<ImageAttachment> {
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!(
            "image is {} MB, limit is {} MB",
            bytes.len() / (1024 * 1024),
            MAX_IMAGE_BYTES / (1024 * 1024)
        );
    }
    Ok(ImageAttachment {
        media_type: media_type.into(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(rgba)?;
    writer.finish()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_png_decodes_to_same_pixels() {
        let rgba = [255u8, 0, 0, 255, 0, 255, 0, 128];
        let png_bytes = encode_png(2, 1, &rgba).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(&buf[..info.buffer_size()], &rgba);
    }

    #[test]
    fn image_path_accepts_quoted_existing_images_only() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("shot.PNG");
        std::fs::write(&img, encode_png(1, 1, &[0, 0, 0, 255]).unwrap()).unwrap();
        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, "x").unwrap();

        let quoted = format!("\"{}\"", img.display());
        assert_eq!(image_path(&quoted), Some(img.display().to_string()));
        assert_eq!(image_path(&txt.display().to_string()), None);
        assert_eq!(
            image_path(&dir.path().join("missing.png").display().to_string()),
            None
        );
        assert_eq!(image_path("hello world"), None);
    }

    #[test]
    fn image_from_path_sets_media_type_and_base64() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("photo.jpeg");
        std::fs::write(&img, b"abc").unwrap();
        let att = image_from_path(&img).unwrap();
        assert_eq!(att.media_type, "image/jpeg");
        assert_eq!(att.data, "YWJj");
        assert_eq!(att.data_url(), "data:image/jpeg;base64,YWJj");
        assert!(image_from_path(&dir.path().join("a.bmp")).is_err());
    }
}
