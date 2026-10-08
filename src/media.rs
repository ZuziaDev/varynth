use anyhow::{bail, Context, Result};
use base64::Engine;
use image::io::Reader as ImageReader;
use image::ImageFormat;
use serde::Serialize;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::config::Config;
use crate::session::ImageAttachment;

/// Maximum encoded media payload accepted by this module.
pub const MAX_MEDIA_BYTES: usize = 8 * 1024 * 1024;
/// Maximum number of media items in one attachment batch.
pub const MAX_MEDIA_COUNT: usize = 8;
/// Maximum combined decoded size of one attachment batch.
pub const MAX_TOTAL_MEDIA_BYTES: usize = 16 * 1024 * 1024;

const MAX_MEDIA_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_ALLOC_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BASE64_BYTES: usize = MAX_MEDIA_BYTES.div_ceil(3) * 4;
const UUID_LENGTH: usize = 36;
const UPLOADS_DIR_NAME: &str = "uploads";

#[derive(Debug, Clone, Serialize)]
pub struct StoredMedia {
    pub id: String,
    pub mime: String,
    pub size: usize,
    pub url: String,
}

#[derive(Debug, Clone, Copy)]
struct MediaKind {
    mime: &'static str,
    format: ImageFormat,
    extension: &'static str,
}

fn media_kind(mime: &str) -> Option<MediaKind> {
    match mime {
        "image/png" => Some(MediaKind {
            mime: "image/png",
            format: ImageFormat::Png,
            extension: "png",
        }),
        "image/jpeg" => Some(MediaKind {
            mime: "image/jpeg",
            format: ImageFormat::Jpeg,
            extension: "jpg",
        }),
        "image/gif" => Some(MediaKind {
            mime: "image/gif",
            format: ImageFormat::Gif,
            extension: "gif",
        }),
        "image/webp" => Some(MediaKind {
            mime: "image/webp",
            format: ImageFormat::WebP,
            extension: "webp",
        }),
        _ => None,
    }
}

/// Validate an image MIME type, byte size, format signature, dimensions, and
/// complete decodability under bounded image-reader limits.
pub fn validate_image(mime: &str, bytes: &[u8]) -> Result<()> {
    let kind = media_kind(mime).ok_or_else(|| anyhow::anyhow!("unsupported image MIME type"))?;
    anyhow::ensure!(!bytes.is_empty(), "image is empty");
    anyhow::ensure!(
        bytes.len() <= MAX_MEDIA_BYTES,
        "image exceeds the {} byte limit",
        MAX_MEDIA_BYTES
    );

    let guessed = image::guess_format(bytes).context("detect image format")?;
    anyhow::ensure!(
        guessed == kind.format,
        "image MIME type does not match its contents"
    );

    let dimensions_reader = ImageReader::with_format(Cursor::new(bytes), kind.format);
    let (width, height) = dimensions_reader
        .into_dimensions()
        .context("read image dimensions")?;
    anyhow::ensure!(width > 0 && height > 0, "image has empty dimensions");
    anyhow::ensure!(
        u64::from(width) <= MAX_MEDIA_PIXELS && u64::from(height) <= MAX_MEDIA_PIXELS,
        "image dimensions exceed the limit"
    );
    anyhow::ensure!(
        u64::from(width).saturating_mul(u64::from(height)) <= MAX_MEDIA_PIXELS,
        "image pixel count exceeds the limit"
    );

    let mut limits = image::io::Limits::default();
    limits.max_image_width = Some(MAX_MEDIA_PIXELS as u32);
    limits.max_image_height = Some(MAX_MEDIA_PIXELS as u32);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC_BYTES);

    let mut reader = ImageReader::with_format(Cursor::new(bytes), kind.format);
    reader.limits(limits);
    reader.decode().context("decode image")?;
    Ok(())
}

/// Validate a provider image attachment after bounded base64 decoding.
pub fn validate_attachment(attachment: &ImageAttachment) -> Result<()> {
    anyhow::ensure!(
        attachment.data.len() <= MAX_BASE64_BYTES,
        "base64 image data exceeds the encoded size limit"
    );
    anyhow::ensure!(
        !attachment.data.is_empty(),
        "image attachment data is empty"
    );

    let bytes = decode_base64(&attachment.data).context("decode image attachment")?;
    anyhow::ensure!(
        bytes.len() <= MAX_MEDIA_BYTES,
        "decoded image exceeds the {} byte limit",
        MAX_MEDIA_BYTES
    );
    validate_image(&attachment.media_type, &bytes)
}

/// Validate and persist an image below `Config::home_dir()/uploads`.
pub fn store_image(mime: &str, bytes: &[u8]) -> Result<StoredMedia> {
    store_image_at(&Config::home_dir(), mime, bytes)
}

/// Testable storage entry point. `home` is the directory under which the
/// `uploads` directory is created and checked.
pub(crate) fn store_image_at(home: &Path, mime: &str, bytes: &[u8]) -> Result<StoredMedia> {
    validate_image(mime, bytes)?;
    let kind = media_kind(mime).expect("validate_image checked the MIME type");
    let (_, uploads, canonical_uploads) = uploads_dir_at(home, true)?;

    for _ in 0..16 {
        let id = Uuid::new_v4().to_string();
        let path = uploads.join(format!("{id}.{}", kind.extension));
        ensure_candidate_parent(&path, &uploads, &canonical_uploads)?;

        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("create media file {}", path.display()))
            }
        };

        let write_result = (|| -> Result<()> {
            file.write_all(bytes).context("write media file")?;
            file.flush().context("flush media file")?;
            file.sync_all().context("sync media file")?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&path);
            return Err(error).with_context(|| format!("store media {}", path.display()));
        }
        drop(file);

        if let Err(error) = ensure_regular_contained_file(&path, &canonical_uploads) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }

        return Ok(StoredMedia {
            id: id.clone(),
            mime: kind.mime.to_string(),
            size: bytes.len(),
            url: format!("/api/media/{id}"),
        });
    }

    bail!("could not allocate a unique media id")
}

/// Read and validate a stored image by its generated UUID.
pub fn read_image(id: &str) -> Result<(String, Vec<u8>)> {
    read_image_at(&Config::home_dir(), id)
}

/// Testable read entry point. `home` is the directory containing `uploads`.
pub(crate) fn read_image_at(home: &Path, id: &str) -> Result<(String, Vec<u8>)> {
    let uuid = parse_media_id(id)?;
    let (_, uploads, canonical_uploads) = uploads_dir_at(home, false)?;
    let mut found = None;

    for kind in MEDIA_KINDS {
        let path = uploads.join(format!("{uuid}.{}", kind.extension));
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure_regular_contained_file(&path, &canonical_uploads)?;
                anyhow::ensure!(
                    metadata.is_file(),
                    "stored media path is not a regular file"
                );
                anyhow::ensure!(found.is_none(), "multiple files exist for media id");
                found = Some((path, kind));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("inspect media {}", path.display()))
            }
        }
    }

    let Some((path, kind)) = found else {
        bail!("media not found")
    };
    let bytes = read_bounded_file(&path)?;
    validate_image(kind.mime, &bytes)?;
    Ok((kind.mime.to_string(), bytes))
}

/// Load stored images into provider-compatible base64 attachments.
pub fn load_attachments(ids: &[String]) -> Result<Vec<ImageAttachment>> {
    load_attachments_at(&Config::home_dir(), ids)
}

/// Testable attachment loader. `home` is the directory containing `uploads`.
pub(crate) fn load_attachments_at(home: &Path, ids: &[String]) -> Result<Vec<ImageAttachment>> {
    anyhow::ensure!(
        ids.len() <= MAX_MEDIA_COUNT,
        "too many media attachments; limit is {}",
        MAX_MEDIA_COUNT
    );

    let mut seen = HashSet::with_capacity(ids.len());
    for id in ids {
        parse_media_id(id)?;
        anyhow::ensure!(seen.insert(id.as_str()), "duplicate media id");
    }

    let mut total = 0usize;
    let mut attachments = Vec::with_capacity(ids.len());
    for id in ids {
        let (mime, bytes) = read_image_at(home, id)?;
        total = total
            .checked_add(bytes.len())
            .ok_or_else(|| anyhow::anyhow!("combined media size overflow"))?;
        anyhow::ensure!(
            total <= MAX_TOTAL_MEDIA_BYTES,
            "combined media exceeds the {} byte limit",
            MAX_TOTAL_MEDIA_BYTES
        );
        attachments.push(ImageAttachment {
            media_type: mime,
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }
    Ok(attachments)
}

const MEDIA_KINDS: [MediaKind; 4] = [
    MediaKind {
        mime: "image/png",
        format: ImageFormat::Png,
        extension: "png",
    },
    MediaKind {
        mime: "image/jpeg",
        format: ImageFormat::Jpeg,
        extension: "jpg",
    },
    MediaKind {
        mime: "image/gif",
        format: ImageFormat::Gif,
        extension: "gif",
    },
    MediaKind {
        mime: "image/webp",
        format: ImageFormat::WebP,
        extension: "webp",
    },
];

fn decode_base64(data: &str) -> Result<Vec<u8>> {
    let standard = base64::engine::general_purpose::STANDARD.decode(data);
    match standard {
        Ok(bytes) => Ok(bytes),
        Err(standard_error) => base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(data)
            .map_err(|_| {
                anyhow::anyhow!("invalid base64 image data; standard decode: {standard_error}")
            }),
    }
}

fn parse_media_id(raw: &str) -> Result<Uuid> {
    anyhow::ensure!(
        raw.len() == UUID_LENGTH
            && raw.as_bytes()[8] == b'-'
            && raw.as_bytes()[13] == b'-'
            && raw.as_bytes()[18] == b'-'
            && raw.as_bytes()[23] == b'-',
        "invalid media id"
    );
    let uuid = Uuid::parse_str(raw).context("invalid media id")?;
    anyhow::ensure!(uuid.to_string() == raw, "invalid media id");
    Ok(uuid)
}

fn uploads_dir_at(home: &Path, create: bool) -> Result<(PathBuf, PathBuf, PathBuf)> {
    anyhow::ensure!(
        !home.as_os_str().is_empty(),
        "media home directory is empty"
    );
    if create {
        fs::create_dir_all(home)
            .with_context(|| format!("create media home {}", home.display()))?;
    }

    let canonical_home = home
        .canonicalize()
        .with_context(|| format!("resolve media home {}", home.display()))?;
    let uploads = home.join(UPLOADS_DIR_NAME);
    match fs::symlink_metadata(&uploads) {
        Ok(metadata) => {
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "media uploads directory must not be a symlink"
            );
            anyhow::ensure!(metadata.is_dir(), "media uploads path is not a directory");
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            fs::create_dir(&uploads)
                .with_context(|| format!("create media uploads {}", uploads.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("media uploads directory does not exist")
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect media uploads {}", uploads.display()))
        }
    }

    let metadata = fs::symlink_metadata(&uploads)
        .with_context(|| format!("inspect media uploads {}", uploads.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink() && metadata.is_dir(),
        "media uploads path is not a regular directory"
    );
    let canonical_uploads = uploads
        .canonicalize()
        .with_context(|| format!("resolve media uploads {}", uploads.display()))?;
    anyhow::ensure!(
        canonical_uploads.starts_with(&canonical_home),
        "media uploads directory escapes the media home"
    );
    Ok((canonical_home, uploads, canonical_uploads))
}

fn ensure_candidate_parent(path: &Path, uploads: &Path, canonical_uploads: &Path) -> Result<()> {
    anyhow::ensure!(
        path.parent() == Some(uploads),
        "media path is outside the uploads directory"
    );
    let canonical_parent = uploads
        .canonicalize()
        .with_context(|| format!("resolve media uploads {}", uploads.display()))?;
    anyhow::ensure!(
        canonical_parent == canonical_uploads,
        "media uploads directory changed during operation"
    );
    Ok(())
}

fn ensure_regular_contained_file(path: &Path, canonical_uploads: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect media file {}", path.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "stored media file must not be a symlink"
    );
    anyhow::ensure!(
        metadata.is_file(),
        "stored media path is not a regular file"
    );
    let canonical_file = path
        .canonicalize()
        .with_context(|| format!("resolve media file {}", path.display()))?;
    anyhow::ensure!(
        canonical_file.parent() == Some(canonical_uploads),
        "stored media file escapes the uploads directory"
    );
    Ok(())
}

fn read_bounded_file(path: &Path) -> Result<Vec<u8>> {
    let metadata =
        fs::metadata(path).with_context(|| format!("stat media file {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "stored media path is not a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_MEDIA_BYTES as u64,
        "stored media exceeds the {} byte limit",
        MAX_MEDIA_BYTES
    );

    let capacity = usize::try_from(metadata.len()).unwrap_or(MAX_MEDIA_BYTES);
    let mut file =
        File::open(path).with_context(|| format!("open media file {}", path.display()))?;
    let mut bytes = Vec::with_capacity(capacity.min(MAX_MEDIA_BYTES));
    std::io::Read::by_ref(&mut file)
        .take((MAX_MEDIA_BYTES as u64) + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read media file {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= MAX_MEDIA_BYTES,
        "stored media exceeds the {} byte limit",
        MAX_MEDIA_BYTES
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn valid_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ])
            .unwrap();
        drop(writer);
        bytes
    }

    #[test]
    fn valid_png_is_decoded_under_limits() {
        validate_image("image/png", &valid_png()).unwrap();
    }

    #[test]
    fn header_only_or_corrupt_png_is_rejected() {
        assert!(validate_image("image/png", b"\x89PNG\r\n\x1a\n").is_err());
        let mut corrupt = valid_png();
        corrupt.truncate(corrupt.len() - 5);
        assert!(validate_image("image/png", &corrupt).is_err());
    }

    #[test]
    fn mime_must_match_detected_format() {
        assert!(validate_image("image/jpeg", &valid_png()).is_err());
    }

    #[test]
    fn oversized_images_are_rejected_before_decode() {
        let bytes = vec![0; MAX_MEDIA_BYTES + 1];
        assert!(validate_image("image/png", &bytes).is_err());
    }

    #[test]
    fn attachment_base64_is_bounded_and_validated() {
        let bytes = valid_png();
        let attachment = ImageAttachment {
            media_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        };
        validate_attachment(&attachment).unwrap();
        let no_padding = ImageAttachment {
            media_type: attachment.media_type.clone(),
            data: attachment.data.trim_end_matches('=').to_string(),
        };
        validate_attachment(&no_padding).unwrap();
        assert!(validate_attachment(&ImageAttachment {
            media_type: "image/png".into(),
            data: "not-base64".into(),
        })
        .is_err());
    }

    #[test]
    fn store_read_and_load_use_only_generated_ids() {
        let dir = tempdir().unwrap();
        let media = store_image_at(dir.path(), "image/png", &valid_png()).unwrap();
        assert_eq!(media.mime, "image/png");
        assert_eq!(media.size, valid_png().len());
        assert_eq!(media.url, format!("/api/media/{}", media.id));

        let (mime, bytes) = read_image_at(dir.path(), &media.id).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(bytes, valid_png());

        let ids = vec![media.id.clone()];
        let attachments = load_attachments_at(dir.path(), &ids).unwrap();
        validate_attachment(&attachments[0]).unwrap();
        assert!(read_image_at(dir.path(), "../escape").is_err());
        assert!(load_attachments_at(dir.path(), &[media.id.clone(), media.id]).is_err());
    }

    #[test]
    fn attachment_count_is_bounded() {
        let dir = tempdir().unwrap();
        let ids = vec![Uuid::new_v4().to_string(); MAX_MEDIA_COUNT + 1];
        assert!(load_attachments_at(dir.path(), &ids).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_uploads_and_files_are_rejected() {
        use std::os::unix::fs::symlink;

        let home = tempdir().unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), home.path().join(UPLOADS_DIR_NAME)).unwrap();
        assert!(store_image_at(home.path(), "image/png", &valid_png()).is_err());

        let home = tempdir().unwrap();
        fs::create_dir(home.path().join(UPLOADS_DIR_NAME)).unwrap();
        let media = store_image_at(home.path(), "image/png", &valid_png()).unwrap();
        let stored = home
            .path()
            .join(UPLOADS_DIR_NAME)
            .join(format!("{}.png", media.id));
        fs::remove_file(&stored).unwrap();
        symlink(outside.path().join("outside.png"), &stored).unwrap();
        assert!(read_image_at(home.path(), &media.id).is_err());
    }
}
