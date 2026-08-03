//! User-initiated screen-region capture.
//!
//! A screen image is never inferred from ambient clipboard state. This entry
//! point is called only from the explicit crop hotkey, and the returned
//! attachment retains [`AttachmentSource::ScreenRegion`] so prompt assembly
//! fences the pixels as untrusted context.

use aibo_core::types::Attachment;
#[cfg(target_os = "macos")]
use aibo_core::types::AttachmentSource;

/// A failure to start or read the platform screen-region picker.
#[derive(Debug, thiserror::Error)]
pub enum ScreenCaptureError {
    /// The platform picker could not be started or its output could not be read.
    #[error("screen-region capture failed: {0}")]
    Io(#[from] std::io::Error),
    /// The picker produced bytes that are not a readable image.
    #[error("screen-region capture produced an unreadable image: {0}")]
    Image(#[from] image::ImageError),
    /// The blocking image probe did not finish normally.
    #[error("screen-region capture worker failed: {0}")]
    Worker(#[from] tokio::task::JoinError),
    /// Interactive region capture is not implemented on this platform.
    #[error("screen-region capture is unavailable on this platform")]
    Unsupported,
}

/// Open the OS region picker and return the chosen pixels.
///
/// `Ok(None)` is cancellation, not a failure. The file lives in a private
/// temporary directory and is removed with it after the bytes have been read.
#[cfg(target_os = "macos")]
pub async fn capture_screen_region() -> Result<Option<Attachment>, ScreenCaptureError> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("screen-region.png");
    let status = tokio::process::Command::new("/usr/sbin/screencapture")
        .args(["-i", "-x", "-t", "png"])
        .arg(&path)
        .status()
        .await?;

    if !status.success() || !tokio::fs::try_exists(&path).await? {
        return Ok(None);
    }
    let bytes = tokio::fs::read(path).await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let probe = bytes.clone();
    let (width, height) = tokio::task::spawn_blocking(move || {
        let image = image::load_from_memory(&probe)?;
        Ok::<_, image::ImageError>((image.width(), image.height()))
    })
    .await??;

    Ok(Some(Attachment::image(
        AttachmentSource::ScreenRegion,
        bytes,
        "image/png",
        width,
        height,
        String::new(),
    )))
}

/// Windows: the Snipping Tool's crop overlay, which copies to the clipboard.
///
/// There is no scriptable crop-to-file tool on Windows, but `ms-screenclip:`
/// opens the same region picker Win+Shift+S does. Launch it, watch the
/// clipboard *sequence number* — Esc never copies, so an unmoved number for
/// the whole window is a cancelled crop, not a failure — then lift the
/// bitmap and re-encode as PNG, the attachment contract.
#[cfg(target_os = "windows")]
pub async fn capture_screen_region() -> Result<Option<Attachment>, ScreenCaptureError> {
    use aibo_core::types::AttachmentSource;
    use std::time::Duration;

    let before = crate::windows::clipboard_sequence();
    // `explorer.exe` returns immediately; the overlay outlives it, so the
    // exit status says nothing about the crop.
    let _ = tokio::process::Command::new("explorer.exe")
        .arg("ms-screenclip:")
        .status()
        .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        if crate::windows::clipboard_sequence() != before {
            break;
        }
    }

    let encoded = tokio::task::spawn_blocking(|| {
        let image = arboard::Clipboard::new()
            .and_then(|mut c| c.get_image())
            .ok()?;
        let width = u32::try_from(image.width).ok()?;
        let height = u32::try_from(image.height).ok()?;
        let buffer = image::RgbaImage::from_raw(width, height, image.bytes.into_owned())?;
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(buffer)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .ok()?;
        Some((png, width, height))
    })
    .await?;
    let Some((bytes, width, height)) = encoded else {
        // The sequence moved but no image followed — the user copied text
        // from somewhere else mid-crop. Treat as cancellation.
        return Ok(None);
    };

    Ok(Some(Attachment::image(
        AttachmentSource::ScreenRegion,
        bytes,
        "image/png",
        width,
        height,
        String::new(),
    )))
}

/// Other builds keep the API available; no default hotkey calls it.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub async fn capture_screen_region() -> Result<Option<Attachment>, ScreenCaptureError> {
    Err(ScreenCaptureError::Unsupported)
}
