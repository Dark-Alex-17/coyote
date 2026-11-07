use super::{base64_encode, get_patch_extension};

use anyhow::{Context, Result, bail};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::path::Path;

/// Anthropic's per-image limit once a request carries more than 20 images.
pub const MAX_IMAGE_DIMENSION: u32 = 2000;
pub const MAX_IMAGE_BASE64_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_IMAGE_SOURCE_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_DECODE_DIMENSION: u32 = 16_384;
pub const MAX_DECODE_ALLOC_BYTES: u64 = 512 * 1024 * 1024;
pub const IMAGE_TOKEN_ESTIMATE: usize = 1600;
pub const MAX_REQUEST_IMAGES: usize = 20;
pub const MAX_REQUEST_IMAGE_BASE64_BYTES: usize = 20 * 1024 * 1024;
pub const REQUEST_IMAGE_LOW_WATERMARK_COUNT: usize = 10;
pub const REQUEST_IMAGE_LOW_WATERMARK_BYTES: usize = 10 * 1024 * 1024;
const _: () = assert!(
    REQUEST_IMAGE_LOW_WATERMARK_COUNT < MAX_REQUEST_IMAGES
        && REQUEST_IMAGE_LOW_WATERMARK_BYTES < MAX_REQUEST_IMAGE_BASE64_BYTES
);
pub const SUPPORTED_IMAGE_FORMATS_NOTE: &str =
    "only PNG, JPEG, WebP and GIF images can be attached";
/// Marks where an attached image sits in a message's text, one per image in
/// attachment order. Typed text, retrieved context and preambles have it
/// stripped, so only the REPL can place an image.
pub const IMAGE_SLOT: char = '\u{FFFC}';

const SUPPORTED_IMAGE_EXTS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];
const SUPPORTED_IMAGE_MIME_TYPES: [&str; 6] = [
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/pjpeg",
    "image/webp",
    "image/gif",
];
const JPEG_FALLBACK_QUALITY: u8 = 85;

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedImage {
    pub mime_type: String,
    pub base64_data: String,
    pub width: u32,
    pub height: u32,
    /// Set only when the image was resized.
    pub original_size: Option<(u32, u32)>,
}

impl PreparedImage {
    pub fn to_data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime_type, self.base64_data)
    }
}

/// How many of the oldest history images a request carrying `count` images
/// must drop. Drops come in whole blocks of the gap between the high and low
/// watermarks, so the cut only moves when the high limit is crossed again and
/// the cached prompt prefix stays the same from one turn to the next.
pub fn history_images_to_drop(count: usize) -> usize {
    overflow_in_blocks(count, MAX_REQUEST_IMAGES, REQUEST_IMAGE_LOW_WATERMARK_COUNT)
}

/// The byte counterpart of `history_images_to_drop`: the oldest history
/// images whose base64 payloads add up to at least this many bytes go.
pub fn history_image_bytes_to_drop(bytes: usize) -> usize {
    overflow_in_blocks(
        bytes,
        MAX_REQUEST_IMAGE_BASE64_BYTES,
        REQUEST_IMAGE_LOW_WATERMARK_BYTES,
    )
}

fn overflow_in_blocks(total: usize, high: usize, low: usize) -> usize {
    let block = high - low;
    total.saturating_sub(high).div_ceil(block) * block
}

/// The placeholder the user sees for the `number`th image (1-based).
pub fn image_marker(number: usize) -> String {
    format!("[Image #{number}]")
}

pub fn strip_image_slots(text: &str) -> String {
    text.replace(IMAGE_SLOT, "")
}

/// Shows each `IMAGE_SLOT` as the `image_marker` of its image. Slots past
/// `image_count` have no image behind them and are dropped.
pub fn render_image_slots(text: &str, image_count: usize) -> String {
    let mut rendered = String::with_capacity(text.len());
    for (index, piece) in text.split(IMAGE_SLOT).enumerate() {
        if index > 0 && index <= image_count {
            rendered.push_str(&image_marker(index));
        }
        rendered.push_str(piece);
    }
    rendered
}

pub fn is_supported_image_path(path: &str) -> bool {
    get_patch_extension(path)
        .map(|ext| SUPPORTED_IMAGE_EXTS.contains(&ext.as_str()))
        .unwrap_or_default()
}

/// Accepts the common JPEG aliases servers send alongside the canonical types.
pub fn is_supported_image_mime(mime_type: &str) -> bool {
    SUPPORTED_IMAGE_MIME_TYPES.contains(&mime_type.to_lowercase().as_str())
}

/// Checks only the leading bytes, so a file that passes can still fail to
/// decode in `prepare_image_file`.
pub fn sniff_supported_image(path: &Path) -> bool {
    // A FIFO would block the open below.
    if !fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut head = Vec::with_capacity(64);
    if file.take(64).read_to_end(&mut head).is_err() {
        return false;
    }
    image::guess_format(&head).is_ok_and(|format| supported_mime_type(format).is_some())
}

fn supported_mime_type(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::Gif => Some("image/gif"),
        ImageFormat::WebP => Some("image/webp"),
        _ => None,
    }
}

pub fn prepare_image_file(path: &Path) -> Result<PreparedImage> {
    let display = path.display();
    // Opening a FIFO for reading blocks until a writer appears, so special
    // files must be rejected from a stat before any open.
    let metadata = fs::metadata(path).with_context(|| format!("Unable to read '{display}'"))?;
    if !metadata.is_file() {
        bail!("'{display}' is not a regular file");
    }
    let file = File::open(path).with_context(|| format!("Unable to open '{display}'"))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("Unable to read '{display}'"))?;
    if !metadata.is_file() {
        bail!("'{display}' is not a regular file");
    }
    if metadata.len() > MAX_IMAGE_SOURCE_BYTES {
        bail!("'{display}' is larger than the {MAX_IMAGE_SOURCE_BYTES} byte image limit");
    }
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Unable to read '{display}'"))?;
    if bytes.len() as u64 > MAX_IMAGE_SOURCE_BYTES {
        bail!("'{display}' is larger than the {MAX_IMAGE_SOURCE_BYTES} byte image limit");
    }
    prepare_image_bytes(&bytes).with_context(|| format!("Invalid image '{display}'"))
}

pub fn prepare_image_bytes(bytes: &[u8]) -> Result<PreparedImage> {
    if bytes.is_empty() {
        bail!("Image is empty");
    }
    let format = image::guess_format(bytes).context("Unrecognized image format")?;
    let Some(mime_type) = supported_mime_type(format) else {
        bail!(
            "Unsupported image format '{}': {SUPPORTED_IMAGE_FORMATS_NOTE}",
            format.to_mime_type()
        );
    };

    // The dimension caps are left off the header read so an oversized image
    // gets an error that names its size rather than a generic limits error.
    let mut header_limits = decode_limits();
    header_limits.max_image_width = None;
    header_limits.max_image_height = None;
    let (width, height) = image_reader(bytes, format, header_limits)
        .into_dimensions()
        .context("Unable to read image header")?;
    if width > MAX_DECODE_DIMENSION || height > MAX_DECODE_DIMENSION {
        bail!(
            "Image is {width}x{height}, larger than the {MAX_DECODE_DIMENSION}x{MAX_DECODE_DIMENSION} limit"
        );
    }

    if width <= MAX_IMAGE_DIMENSION && height <= MAX_IMAGE_DIMENSION {
        let base64_data = base64_encode(bytes);
        if base64_data.len() <= MAX_IMAGE_BASE64_BYTES {
            return Ok(PreparedImage {
                mime_type: mime_type.into(),
                base64_data,
                width,
                height,
                original_size: None,
            });
        }
    }

    let mut decoder = image_reader(bytes, format, decode_limits())
        .into_decoder()
        .context("Unable to decode image")?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).context("Unable to decode image")?;
    image.apply_orientation(orientation);
    let (width, height) = (image.width(), image.height());
    let (target_width, target_height) = fit_within(width, height, MAX_IMAGE_DIMENSION);
    let image = if (target_width, target_height) == (width, height) {
        image
    } else {
        image.resize_exact(target_width, target_height, FilterType::Triangle)
    };
    let original_size =
        ((image.width(), image.height()) != (width, height)).then_some((width, height));

    let (mime_type, base64_data) = encode_within_cap(&image)?;
    Ok(PreparedImage {
        mime_type: mime_type.into(),
        base64_data,
        width: image.width(),
        height: image.height(),
        original_size,
    })
}

fn decode_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    limits
}

fn image_reader(bytes: &[u8], format: ImageFormat, limits: Limits) -> ImageReader<Cursor<&[u8]>> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    reader
}

fn fit_within(width: u32, height: u32, max: u32) -> (u32, u32) {
    if width <= max && height <= max {
        return (width, height);
    }
    let longest = u64::from(width.max(height));
    let scale = |side: u32| {
        let scaled = (u64::from(side) * u64::from(max) + longest / 2) / longest;
        (scaled as u32).max(1)
    };
    (scale(width), scale(height))
}

fn encode_within_cap(image: &DynamicImage) -> Result<(&'static str, String)> {
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, ImageFormat::Png)
        .context("Unable to encode image as PNG")?;
    let base64_data = base64_encode(png.get_ref());
    if base64_data.len() <= MAX_IMAGE_BASE64_BYTES {
        return Ok(("image/png", base64_data));
    }

    let mut jpeg = Vec::new();
    DynamicImage::ImageRgb8(image.to_rgb8())
        .write_with_encoder(JpegEncoder::new_with_quality(
            &mut jpeg,
            JPEG_FALLBACK_QUALITY,
        ))
        .context("Unable to encode image as JPEG")?;
    let base64_data = base64_encode(&jpeg);
    if base64_data.len() <= MAX_IMAGE_BASE64_BYTES {
        return Ok(("image/jpeg", base64_data));
    }
    bail!(
        "Image is still larger than the {MAX_IMAGE_BASE64_BYTES} byte limit after resizing and JPEG compression"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::temp_file;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, GenericImageView, RgbImage, RgbaImage};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = temp_file("-image-prep-", "");
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn encode(image: DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        image.write_to(&mut buf, format).unwrap();
        buf.into_inner()
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        });
        encode(DynamicImage::ImageRgba8(image), ImageFormat::Png)
    }

    fn jpeg_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 64])
        });
        encode(DynamicImage::ImageRgb8(image), ImageFormat::Jpeg)
    }

    fn gif_bytes(width: u32, height: u32, frames: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut buf);
            for i in 0..frames {
                let image = RgbaImage::from_fn(width, height, |x, y| {
                    image::Rgba([(x % 256) as u8, (y % 256) as u8, (i * 80) as u8, 255])
                });
                let delay = Delay::from_saturating_duration(Duration::from_millis(100));
                encoder
                    .encode_frame(Frame::from_parts(image, 0, 0, delay))
                    .unwrap();
            }
        }
        buf
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }

    #[test]
    fn to_data_url_formats_mime_and_payload() {
        let image = PreparedImage {
            mime_type: "image/png".into(),
            base64_data: "abc".into(),
            width: 1,
            height: 1,
            original_size: None,
        };
        assert_eq!(image.to_data_url(), "data:image/png;base64,abc");
    }

    #[test]
    fn image_slots_render_as_numbered_markers_or_strip() {
        let text = format!("{IMAGE_SLOT} a {IMAGE_SLOT}b{IMAGE_SLOT}");

        assert_eq!(
            render_image_slots(&text, 3),
            "[Image #1] a [Image #2]b[Image #3]"
        );
        assert_eq!(strip_image_slots(&text), " a b");
        assert_eq!(render_image_slots("no slots", 0), "no slots");
    }

    #[test]
    fn image_slots_past_the_image_count_are_dropped() {
        let text = format!("{IMAGE_SLOT} a {IMAGE_SLOT}b{IMAGE_SLOT}");

        assert_eq!(render_image_slots(&text, 1), "[Image #1] a b");
        assert_eq!(render_image_slots(&text, 0), " a b");
    }

    #[test]
    fn is_supported_image_path_ignores_case() {
        assert!(is_supported_image_path("photo.png"));
        assert!(is_supported_image_path("PHOTO.JPG"));
        assert!(is_supported_image_path("dir with space/a.WebP"));
        assert!(!is_supported_image_path("image.svg"));
        assert!(!is_supported_image_path("png"));
    }

    #[test]
    fn history_images_drop_in_blocks_of_the_watermark_gap() {
        let dropped: Vec<_> = [0, 20, 21, 25, 30, 31, 40, 41]
            .into_iter()
            .map(history_images_to_drop)
            .collect();

        assert_eq!(dropped, [0, 0, 10, 10, 10, 20, 20, 30]);
    }

    #[test]
    fn history_image_bytes_drop_in_blocks_of_the_watermark_gap() {
        let mb = 1024 * 1024;

        assert_eq!(history_image_bytes_to_drop(20 * mb), 0);
        assert_eq!(history_image_bytes_to_drop(20 * mb + 1), 10 * mb);
        assert_eq!(history_image_bytes_to_drop(30 * mb), 10 * mb);
        assert_eq!(history_image_bytes_to_drop(30 * mb + 1), 20 * mb);
    }

    #[test]
    fn sniff_supported_image_checks_content_not_name() {
        let dir = TempDir::new();
        let jpeg = dir.path("photo.png");
        fs::write(&jpeg, jpeg_bytes(8, 8)).unwrap();
        let text = dir.path("notes.png");
        fs::write(&text, "just some text, not an image").unwrap();
        let bmp = dir.path("old.png");
        fs::write(&bmp, b"BM\0\0\0\0\0\0\0\0\0\0\0\0").unwrap();
        let folder = dir.path("photos.png");
        fs::create_dir(&folder).unwrap();

        assert!(sniff_supported_image(&jpeg));
        assert!(!sniff_supported_image(&text));
        assert!(!sniff_supported_image(&bmp));
        assert!(!sniff_supported_image(&folder));
        assert!(!sniff_supported_image(&dir.path("missing.png")));
    }

    #[test]
    fn wide_png_is_resized_to_fit() {
        let prepared = prepare_image_bytes(&png_bytes(4000, 1000)).unwrap();
        assert_eq!((prepared.width, prepared.height), (2000, 500));
        assert_eq!(prepared.original_size, Some((4000, 1000)));
        assert_eq!(prepared.mime_type, "image/png");
        let decoded =
            image::load_from_memory(&crate::utils::base64_decode(&prepared.base64_data).unwrap())
                .unwrap();
        assert_eq!(decoded.dimensions(), (2000, 500));
    }

    #[test]
    fn exif_rotated_jpeg_is_oriented_before_fitting() {
        let mut jpeg = Vec::new();
        let image = RgbImage::from_pixel(3000, 1000, image::Rgb([200, 40, 40]));
        JpegEncoder::new_with_quality(&mut jpeg, 80)
            .encode_image(&image)
            .unwrap();

        let mut exif = b"Exif\0\0MM\0*\0\0\0\x08".to_vec();
        exif.extend_from_slice(&1u16.to_be_bytes());
        // Orientation (0x0112), SHORT, count 1, value 6: rotate 90 degrees clockwise.
        exif.extend_from_slice(&[0x01, 0x12, 0x00, 0x03, 0, 0, 0, 1, 0x00, 0x06, 0, 0]);
        exif.extend_from_slice(&0u32.to_be_bytes());
        let mut app1 = vec![0xff, 0xe1];
        app1.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        app1.extend_from_slice(&exif);
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        jpeg.splice(2..2, app1);

        let prepared = prepare_image_bytes(&jpeg).unwrap();
        assert!(prepared.height > prepared.width);
        assert_eq!(prepared.height, 2000);
        assert!((666..=667).contains(&prepared.width), "{}", prepared.width);
        assert_eq!(prepared.original_size, Some((1000, 3000)));
    }

    #[test]
    fn small_png_passes_through_unchanged() {
        let bytes = png_bytes(100, 100);
        let prepared = prepare_image_bytes(&bytes).unwrap();
        assert_eq!(prepared.base64_data, base64_encode(&bytes));
        assert_eq!(prepared.mime_type, "image/png");
        assert_eq!((prepared.width, prepared.height), (100, 100));
        assert_eq!(prepared.original_size, None);
    }

    #[test]
    fn jpeg_named_png_is_typed_by_content() {
        let dir = TempDir::new();
        let path = dir.path("x.png");
        fs::write(&path, jpeg_bytes(64, 32)).unwrap();
        let prepared = prepare_image_file(&path).unwrap();
        assert_eq!(prepared.mime_type, "image/jpeg");
        assert_eq!((prepared.width, prepared.height), (64, 32));
    }

    #[test]
    fn text_file_named_png_fails_naming_path() {
        let dir = TempDir::new();
        let path = dir.path("x.png");
        fs::write(&path, "just some text, not an image").unwrap();
        let err = prepare_image_file(&path).unwrap_err();
        assert!(format!("{err:#}").contains(&path.display().to_string()));
    }

    #[test]
    fn directory_is_rejected() {
        let dir = TempDir::new();
        let path = dir.path("photos.png");
        fs::create_dir(&path).unwrap();
        let err = prepare_image_file(&path).unwrap_err();
        assert!(format!("{err:#}").contains("not a regular file"));
    }

    #[test]
    fn oversize_file_is_rejected() {
        let dir = TempDir::new();
        let path = dir.path("huge.png");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_IMAGE_SOURCE_BYTES + 10).unwrap();
        drop(file);
        let err = prepare_image_file(&path).unwrap_err();
        assert!(format!("{err:#}").contains("larger than"));
    }

    #[test]
    fn empty_input_is_rejected() {
        assert!(prepare_image_bytes(&[]).is_err());
        let dir = TempDir::new();
        let path = dir.path("empty.png");
        fs::write(&path, b"").unwrap();
        assert!(prepare_image_file(&path).is_err());
    }

    #[test]
    fn huge_declared_dimensions_rejected_from_header() {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&100_000u32.to_be_bytes());
        ihdr.extend_from_slice(&100_000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        png_chunk(&mut bytes, b"IHDR", &ihdr);
        png_chunk(&mut bytes, b"IDAT", &[]);
        png_chunk(&mut bytes, b"IEND", &[]);

        let started = Instant::now();
        let err = prepare_image_bytes(&bytes).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(format!("{err:#}").contains("100000x100000"), "{err:#}");
    }

    #[test]
    fn noisy_rgba_falls_back_to_jpeg() {
        let mut state = 0x2545_f491u32;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        };
        let image = RgbaImage::from_fn(2000, 2000, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 96, next()])
        });
        let bytes = encode(DynamicImage::ImageRgba8(image), ImageFormat::Png);
        assert!(base64_encode(&bytes).len() > MAX_IMAGE_BASE64_BYTES);

        let prepared = prepare_image_bytes(&bytes).unwrap();
        assert_eq!(prepared.mime_type, "image/jpeg");
        assert!(prepared.base64_data.len() <= MAX_IMAGE_BASE64_BYTES);
        assert_eq!((prepared.width, prepared.height), (2000, 2000));
        assert_eq!(prepared.original_size, None);
    }

    #[test]
    fn small_animated_gif_passes_through_unchanged() {
        let bytes = gif_bytes(32, 32, 2);
        let prepared = prepare_image_bytes(&bytes).unwrap();
        assert_eq!(prepared.mime_type, "image/gif");
        assert_eq!(prepared.base64_data, base64_encode(&bytes));
        assert_eq!(prepared.original_size, None);
    }

    #[test]
    fn large_animated_gif_is_resized_to_png() {
        let bytes = gif_bytes(2400, 100, 2);
        let prepared = prepare_image_bytes(&bytes).unwrap();
        assert_eq!(prepared.mime_type, "image/png");
        assert_eq!((prepared.width, prepared.height), (2000, 83));
        assert_eq!(prepared.original_size, Some((2400, 100)));
    }

    #[test]
    fn tall_narrow_image_keeps_one_pixel_width() {
        let prepared = prepare_image_bytes(&png_bytes(1, 10_000)).unwrap();
        assert_eq!((prepared.width, prepared.height), (1, 2000));
        assert_eq!(prepared.original_size, Some((1, 10_000)));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_rejected_without_blocking() {
        let dir = TempDir::new();
        let path = dir.path("pipe.png");
        let created = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !created {
            return;
        }
        let started = Instant::now();
        let err = prepare_image_file(&path).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(format!("{err:#}").contains("not a regular file"));
    }
}
