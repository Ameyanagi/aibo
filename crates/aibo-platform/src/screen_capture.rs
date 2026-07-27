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

/// Non-macOS builds keep the API available; no default hotkey calls it.
#[cfg(not(target_os = "macos"))]
pub async fn capture_screen_region() -> Result<Option<Attachment>, ScreenCaptureError> {
    Err(ScreenCaptureError::Unsupported)
}
