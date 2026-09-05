//! Native port of pinned Pi's utils/{mime,image-process,image-resize-core,
//! image-convert,exif-orientation}.ts. See PI_UPSTREAM.md and image fixtures.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageOutputFormat, RgbaImage, imageops};
use std::io::Cursor;

pub const CONVERSION_FAILED: &str =
    "[Image omitted: could not be converted to a supported inline image format.]";
pub const RESIZE_FAILED: &str =
    "[Image omitted: could not be resized below the inline image size limit.]";

#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResizeOptions {
    pub max_width: u32,
    pub max_height: u32,
    pub max_bytes: usize,
    pub jpeg_quality: u8,
}
impl Default for ResizeOptions {
    fn default() -> Self {
        Self {
            max_width: 2000,
            max_height: 2000,
            max_bytes: 4_718_592,
            jpeg_quality: 80,
        }
    }
}

#[derive(Debug)]
pub struct ProcessedImage {
    pub data: String,
    pub mime_type: String,
    pub hints: Vec<String>,
}

fn u16le(bytes: &[u8], at: usize) -> u16 {
    bytes.get(at).copied().unwrap_or(0) as u16
        | (bytes.get(at + 1).copied().unwrap_or(0) as u16) << 8
}
fn u32le(bytes: &[u8], at: usize) -> u32 {
    (0..4)
        .map(|n| (bytes.get(at + n).copied().unwrap_or(0) as u32) << (8 * n))
        .sum()
}
fn u32be(bytes: &[u8], at: usize) -> u32 {
    (0..4)
        .map(|n| (bytes.get(at + n).copied().unwrap_or(0) as u32) << (8 * (3 - n)))
        .sum()
}
fn is_at(bytes: &[u8], at: usize, value: &[u8]) -> bool {
    bytes.get(at..at.saturating_add(value.len())) == Some(value)
}

pub fn detect_mime(bytes: &[u8]) -> Option<&'static str> {
    // Pi reads only the first 4100 bytes when detecting a file's format.
    let bytes = &bytes[..bytes.len().min(4100)];
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return (bytes.get(3) != Some(&0xf7)).then_some("image/jpeg");
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        if bytes.len() < 16 || u32be(bytes, 8) != 13 || !is_at(bytes, 12, b"IHDR") {
            return None;
        }
        let mut offset = 8;
        while offset + 8 <= bytes.len() {
            if is_at(bytes, offset + 4, b"acTL") {
                return None;
            }
            if is_at(bytes, offset + 4, b"IDAT") {
                break;
            }
            let next = offset + 12 + u32be(bytes, offset) as usize;
            if next > bytes.len() {
                break;
            }
            offset = next;
        }
        return Some("image/png");
    }
    if bytes.starts_with(b"GIF") {
        return Some("image/gif");
    }
    if bytes.starts_with(b"RIFF") && is_at(bytes, 8, b"WEBP") {
        return Some("image/webp");
    }
    if bytes.starts_with(b"BM") && bytes.len() >= 26 {
        let size = u32le(bytes, 2);
        let pixels = u32le(bytes, 10);
        let dib = u32le(bytes, 14);
        if (size != 0 && size < 26)
            || (pixels as u64) < 14 + dib as u64
            || (size != 0 && pixels >= size)
        {
            return None;
        }
        let (planes, bits) = if dib == 12 {
            (u16le(bytes, 22), u16le(bytes, 24))
        } else if (40..=124).contains(&dib) && bytes.len() >= 30 {
            (u16le(bytes, 26), u16le(bytes, 28))
        } else {
            return None;
        };
        if planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits) {
            return Some("image/bmp");
        }
    }
    None
}

fn exif_orientation(bytes: &[u8]) -> u16 {
    let mut tiff = None;
    if bytes.starts_with(&[0xff, 0xd8]) {
        let mut offset = 2;
        while offset + 1 < bytes.len() {
            if bytes[offset] != 0xff {
                break;
            }
            let marker = bytes[offset + 1];
            if marker == 0xff {
                offset += 1;
                continue;
            }
            if marker == 0xe1 && is_at(bytes, offset + 4, b"Exif\0\0") {
                tiff = Some(offset + 10);
                break;
            }
            if offset + 4 > bytes.len() {
                break;
            }
            offset += 2 + u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        }
    } else if bytes.starts_with(b"RIFF") && is_at(bytes, 8, b"WEBP") {
        let mut offset = 12;
        while offset + 8 <= bytes.len() {
            let size = u32le(bytes, offset + 4) as usize;
            let start = offset + 8;
            if is_at(bytes, offset, b"EXIF") {
                if start + size <= bytes.len() {
                    tiff = Some(
                        start
                            + if size >= 6 && is_at(bytes, start, b"Exif\0\0") {
                                6
                            } else {
                                0
                            },
                    );
                }
                break;
            }
            offset = start + size + size % 2;
        }
    }
    let Some(start) = tiff.filter(|at| at + 8 <= bytes.len()) else {
        return 1;
    };
    let le = is_at(bytes, start, b"II");
    let read16 = |at: usize| {
        if le {
            u16le(bytes, at)
        } else {
            u16::from_be_bytes([
                bytes.get(at).copied().unwrap_or(0),
                bytes.get(at + 1).copied().unwrap_or(0),
            ])
        }
    };
    let ifd = start
        + if le {
            u32le(bytes, start + 4)
        } else {
            u32be(bytes, start + 4)
        } as usize;
    if ifd + 2 > bytes.len() {
        return 1;
    }
    for i in 0..read16(ifd) as usize {
        let entry = ifd + 2 + i * 12;
        if entry + 12 > bytes.len() {
            return 1;
        }
        if read16(entry) == 0x0112 {
            let value = read16(entry + 8);
            return if (1..=8).contains(&value) { value } else { 1 };
        }
    }
    1
}

fn decode(bytes: &[u8]) -> Option<RgbaImage> {
    let image = image::load_from_memory(bytes).ok()?.to_rgba8();
    Some(match exif_orientation(bytes) {
        2 => imageops::flip_horizontal(&image),
        3 => imageops::rotate180(&image),
        4 => imageops::flip_vertical(&image),
        5 => imageops::flip_horizontal(&imageops::rotate90(&image)),
        6 => imageops::rotate90(&image),
        7 => imageops::flip_horizontal(&imageops::rotate270(&image)),
        8 => imageops::rotate270(&image),
        _ => image,
    })
}
fn encode(image: &RgbaImage, format: ImageOutputFormat) -> Option<Vec<u8>> {
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image.clone())
        .write_to(&mut output, format)
        .ok()?;
    Some(output.into_inner())
}

pub fn process(
    bytes: &[u8],
    mime: &str,
    auto_resize: bool,
    options: ResizeOptions,
) -> Result<ProcessedImage, &'static str> {
    let base = mime.split(';').next().unwrap_or(mime).trim().to_lowercase();
    let normalized;
    let (bytes, mime, converted) = match base.as_str() {
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" => (bytes, base.as_str(), false),
        "image/jpg" => (bytes, "image/jpeg", false),
        _ => {
            normalized = decode(bytes)
                .and_then(|image| encode(&image, ImageOutputFormat::Png))
                .ok_or(CONVERSION_FAILED)?;
            (normalized.as_slice(), "image/png", true)
        }
    };
    let mut result = ProcessedImage {
        data: STANDARD.encode(bytes),
        mime_type: mime.into(),
        hints: Vec::new(),
    };
    if auto_resize {
        let image = decode(bytes).ok_or(RESIZE_FAILED)?;
        let (original_width, original_height) = image.dimensions();
        if original_width > options.max_width
            || original_height > options.max_height
            || result.data.len() >= options.max_bytes
        {
            let (mut width, mut height) = (original_width, original_height);
            if width > options.max_width {
                height = (height as f64 * options.max_width as f64 / width as f64).round() as u32;
                width = options.max_width;
            }
            if height > options.max_height {
                width = (width as f64 * options.max_height as f64 / height as f64).round() as u32;
                height = options.max_height;
            }
            let mut qualities = Vec::new();
            for quality in [options.jpeg_quality, 85, 70, 55, 40] {
                if !qualities.contains(&quality) {
                    qualities.push(quality);
                }
            }
            'resize: loop {
                let resized =
                    imageops::resize(&image, width, height, imageops::FilterType::Lanczos3);
                // Pi selects the first fitting candidate, PNG before JPEG; its
                // comment about choosing the smaller format is not its behavior.
                for format in std::iter::once(ImageOutputFormat::Png)
                    .chain(qualities.iter().copied().map(ImageOutputFormat::Jpeg))
                {
                    let mime = if matches!(format, ImageOutputFormat::Png) {
                        "image/png"
                    } else {
                        "image/jpeg"
                    };
                    let data = STANDARD.encode(encode(&resized, format).ok_or(RESIZE_FAILED)?);
                    if data.len() < options.max_bytes {
                        result.data = data;
                        result.mime_type = mime.into();
                        result.hints.push(format!("[Image: original {original_width}x{original_height}, displayed at {width}x{height}. Multiply coordinates by {} to map to original image.]", crate::usage::fixed(original_width as f64 / width as f64, 2)));
                        break 'resize;
                    }
                }
                if width == 1 && height == 1 {
                    return Err(RESIZE_FAILED);
                }
                let next_width = ((width as f64 * 0.75).floor() as u32).max(1);
                let next_height = ((height as f64 * 0.75).floor() as u32).max(1);
                if next_width == width && next_height == height {
                    return Err(RESIZE_FAILED);
                }
                (width, height) = (next_width, next_height);
            }
        }
    }
    if converted && base != result.mime_type {
        result.hints.insert(
            0,
            format!("[Image converted from {base} to {}.]", result.mime_type),
        );
    }
    Ok(result)
}
