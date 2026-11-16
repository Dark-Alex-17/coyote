use anyhow::Context;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    ImageFiles(Vec<PathBuf>),
    /// Copied files, none of them an image. Carries how many were copied.
    NonImageFiles(usize),
    ImagePng(Vec<u8>),
    Text(String),
    Empty,
}

/// Files win over image data, since a file manager copy also carries a
/// rendered icon as image data. For the same reason a copy of only non-image
/// files never reads image data and falls back to text instead. The image and
/// text reads are closures so a stage that is not reached never touches the
/// clipboard.
pub fn classify_clipboard(
    files: Vec<PathBuf>,
    is_image: impl Fn(&Path) -> bool,
    image: impl FnOnce() -> Option<Vec<u8>>,
    text: impl FnOnce() -> Option<String>,
) -> ClipboardContent {
    let non_empty_text = || text().filter(|text| !text.is_empty());
    if files.is_empty() {
        return match image() {
            Some(png) => ClipboardContent::ImagePng(png),
            None => non_empty_text().map_or(ClipboardContent::Empty, ClipboardContent::Text),
        };
    }
    let copied = files.len();
    let images: Vec<PathBuf> = files.into_iter().filter(|path| is_image(path)).collect();
    if !images.is_empty() {
        return ClipboardContent::ImageFiles(images);
    }
    non_empty_text().map_or(
        ClipboardContent::NonImageFiles(copied),
        ClipboardContent::Text,
    )
}

#[cfg(not(any(target_os = "android", target_os = "emscripten")))]
mod internal {
    use super::{ClipboardContent, classify_clipboard};
    use arboard::Clipboard;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::sync::{LazyLock, Mutex};

    static CLIPBOARD: LazyLock<Mutex<Option<Clipboard>>> =
        LazyLock::new(|| Mutex::new(Clipboard::new().ok()));

    pub fn set_text(text: &str) -> anyhow::Result<()> {
        let mut clipboard = CLIPBOARD.lock().unwrap();
        match clipboard.as_mut() {
            Some(clipboard) => {
                clipboard.set_text(text)?;
                #[cfg(target_os = "linux")]
                std::thread::sleep(std::time::Duration::from_millis(50));
                Ok(())
            }
            None => set_text_osc52(text),
        }
    }

    /// A read that fails or comes back empty counts as absent, so the next
    /// stage gets its turn. Only a missing clipboard is an error.
    pub fn read_for_paste() -> anyhow::Result<ClipboardContent> {
        let mut clipboard = CLIPBOARD.lock().unwrap();
        let Some(clipboard) = clipboard.as_mut() else {
            return Err(anyhow::anyhow!("no system clipboard"));
        };

        let files = clipboard.get().file_list().unwrap_or_default();
        let is_image = |path: &std::path::Path| {
            path.to_str()
                .is_some_and(crate::utils::is_supported_image_path)
                && crate::utils::sniff_supported_image(path)
        };
        let clipboard = RefCell::new(clipboard);
        Ok(classify_clipboard(
            files,
            is_image,
            || clipboard.borrow_mut().get_image().ok().and_then(encode_png),
            || clipboard.borrow_mut().get_text().ok(),
        ))
    }

    fn encode_png(image: arboard::ImageData) -> Option<Vec<u8>> {
        let rgba = image::RgbaImage::from_raw(
            image.width as u32,
            image.height as u32,
            image.bytes.into_owned(),
        )?;
        let mut cursor = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .ok()?;
        Some(cursor.into_inner())
    }

    /// Attempts to set text to clipboard with OSC52 escape sequence
    /// Works in many modern terminals, including over SSH.
    fn set_text_osc52(text: &str) -> anyhow::Result<()> {
        let encoded = STANDARD.encode(text);
        let seq = format!("\x1b]52;c;{encoded}\x07");
        if let Err(e) = std::io::Write::write_all(&mut std::io::stdout(), seq.as_bytes()) {
            return Err(anyhow::anyhow!("Failed to send OSC52 sequence").context(e));
        }
        if let Err(e) = std::io::Write::flush(&mut std::io::stdout()) {
            return Err(anyhow::anyhow!("Failed to flush OSC52 sequence").context(e));
        }
        Ok(())
    }
}

#[cfg(any(target_os = "android", target_os = "emscripten"))]
mod internal {
    use super::ClipboardContent;

    pub fn set_text(_text: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("No clipboard available"))
    }

    pub fn read_for_paste() -> anyhow::Result<ClipboardContent> {
        Err(anyhow::anyhow!("No clipboard available"))
    }
}

pub fn set_text(text: &str) -> anyhow::Result<()> {
    internal::set_text(text).context("Failed to copy")
}

pub fn read_for_paste() -> anyhow::Result<ClipboardContent> {
    internal::read_for_paste()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_png(path: &Path) -> bool {
        path.extension().is_some_and(|ext| ext == "png")
    }

    #[test]
    fn image_files_win_and_skip_the_other_reads() {
        let files = vec![PathBuf::from("a.png"), PathBuf::from("notes.txt")];

        assert_eq!(
            classify_clipboard(
                files,
                is_png,
                || panic!("image data must not be read"),
                || panic!("text must not be read"),
            ),
            ClipboardContent::ImageFiles(vec![PathBuf::from("a.png")])
        );
    }

    #[test]
    fn non_image_files_without_text_are_reported() {
        let files = vec![PathBuf::from("a.txt"), PathBuf::from("b.pdf")];

        assert_eq!(
            classify_clipboard(
                files,
                is_png,
                || panic!("image data must not be read"),
                || { Some(String::new()) }
            ),
            ClipboardContent::NonImageFiles(2)
        );
    }

    #[test]
    fn non_image_files_fall_back_to_text_without_reading_image_data() {
        let files = vec![PathBuf::from("a.txt")];

        assert_eq!(
            classify_clipboard(
                files,
                is_png,
                || panic!("image data must not be read"),
                || { Some("a.txt".into()) }
            ),
            ClipboardContent::Text("a.txt".into())
        );
    }

    #[test]
    fn image_data_is_used_when_no_files_were_copied() {
        assert_eq!(
            classify_clipboard(
                Vec::new(),
                is_png,
                || Some(vec![1, 2, 3]),
                || panic!("text must not be read"),
            ),
            ClipboardContent::ImagePng(vec![1, 2, 3])
        );
    }
}
