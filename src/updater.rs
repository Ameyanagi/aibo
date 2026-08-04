//! Check, download and verify GitHub release artifacts.
//!
//! Downloads never execute by themselves. The UI must issue an explicit
//! install request after this module has verified the release's companion
//! SHA-256 file.

use std::path::{Path, PathBuf};

use aibo_ui::bridge::UpdateChannel;
use futures::StreamExt as _;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

const REPOSITORY_API: &str = "https://api.github.com/repos/Ameyanagi/aibo";
const MAX_UPDATE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub enum Outcome {
    UpToDate,
    Ready(VerifiedUpdate),
}

#[derive(Debug)]
pub struct VerifiedUpdate {
    pub version: String,
    pub path: PathBuf,
    pub sha256: String,
}

pub async fn check_and_download(
    channel: UpdateChannel,
    destination: &Path,
    on_download: impl FnOnce(),
) -> Result<Outcome, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("aibo/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let release_url = match channel {
        UpdateChannel::Stable => format!("{REPOSITORY_API}/releases/latest"),
        UpdateChannel::Nightly => format!("{REPOSITORY_API}/releases/tags/dev"),
    };
    let release = get_json(&client, &release_url).await?;
    let tag = release["tag_name"]
        .as_str()
        .ok_or_else(|| "release response had no tag".to_owned())?;
    let version = match channel {
        UpdateChannel::Stable => tag.trim_start_matches('v').to_owned(),
        UpdateChannel::Nightly => {
            let reference =
                get_json(&client, &format!("{REPOSITORY_API}/git/ref/tags/dev")).await?;
            let sha = reference["object"]["sha"]
                .as_str()
                .ok_or_else(|| "nightly release had no commit".to_owned())?;
            format!("0.0.0-dev.{}", &sha[..sha.len().min(7)])
        }
    };
    let current = option_env!("AIBO_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    if current == version {
        return Ok(Outcome::UpToDate);
    }
    on_download();

    let asset_name = platform_asset_name()
        .ok_or_else(|| "automatic updates are not available on this platform".to_owned())?;
    let checksum_name = format!("{asset_name}.sha256");
    let assets = release["assets"]
        .as_array()
        .ok_or_else(|| "release response had no assets".to_owned())?;
    let asset = find_asset(assets, asset_name)?;
    let checksum_asset = find_asset(assets, &checksum_name)?;
    let size = asset["size"]
        .as_u64()
        .ok_or_else(|| "release asset had no size".to_owned())?;
    if size == 0 || size > MAX_UPDATE_BYTES {
        return Err(format!(
            "release asset size {size} is outside the allowed range"
        ));
    }

    let checksum_text = get_text(
        &client,
        checksum_asset["browser_download_url"]
            .as_str()
            .ok_or_else(|| "checksum asset had no download URL".to_owned())?,
    )
    .await?;
    let expected = parse_sha256(&checksum_text)?;
    tokio::fs::create_dir_all(destination)
        .await
        .map_err(|error| error.to_string())?;
    let safe_version: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let final_path = destination.join(format!("{safe_version}-{asset_name}"));
    let partial_path = destination.join(format!("{safe_version}-{asset_name}.partial"));
    let url = asset["browser_download_url"]
        .as_str()
        .ok_or_else(|| "release asset had no download URL".to_owned())?;
    let response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| error.to_string())?;
    let mut stream = response.bytes_stream();
    let mut file = tokio::fs::File::create(&partial_path)
        .await
        .map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut written = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        written = written.saturating_add(chunk.len() as u64);
        if written > MAX_UPDATE_BYTES || written > size.saturating_add(1024) {
            let _ = tokio::fs::remove_file(&partial_path).await;
            return Err("download exceeded the advertised size".to_owned());
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|error| error.to_string())?;
    }
    file.flush().await.map_err(|error| error.to_string())?;
    drop(file);
    if written != size {
        let _ = tokio::fs::remove_file(&partial_path).await;
        return Err(format!(
            "downloaded {written} bytes; release advertised {size}"
        ));
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        let _ = tokio::fs::remove_file(&partial_path).await;
        return Err("download checksum did not match the release".to_owned());
    }
    if tokio::fs::try_exists(&final_path).await.unwrap_or(false) {
        tokio::fs::remove_file(&final_path)
            .await
            .map_err(|error| error.to_string())?;
    }
    tokio::fs::rename(&partial_path, &final_path)
        .await
        .map_err(|error| error.to_string())?;
    Ok(Outcome::Ready(VerifiedUpdate {
        version,
        path: final_path,
        sha256: actual,
    }))
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<serde_json::Value, String> {
    client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| error.to_string())?
        .json()
        .await
        .map_err(|error| error.to_string())
}

async fn get_text(client: &reqwest::Client, url: &str) -> Result<String, String> {
    client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| error.to_string())?
        .text()
        .await
        .map_err(|error| error.to_string())
}

fn find_asset<'a>(
    assets: &'a [serde_json::Value],
    name: &str,
) -> Result<&'a serde_json::Value, String> {
    assets
        .iter()
        .find(|asset| asset["name"].as_str() == Some(name))
        .ok_or_else(|| format!("release has no {name} asset"))
}

fn parse_sha256(text: &str) -> Result<String, String> {
    let digest = text
        .split_ascii_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(digest)
        .ok_or_else(|| "release checksum was malformed".to_owned())
}

const fn platform_asset_name() -> Option<&'static str> {
    if cfg!(target_os = "windows") {
        Some("aibo-windows-x86_64-setup.exe")
    } else if cfg!(target_os = "macos") {
        Some("aibo-macos-aarch64.dmg")
    } else {
        None
    }
}

pub async fn install(update: &VerifiedUpdate) -> Result<(), String> {
    // A file in the staging directory can be modified after download. Verify
    // it again at the last responsible moment rather than treating a path as
    // proof that the bytes are still the ones GitHub published.
    verify_staged(update).await?;

    #[cfg(windows)]
    {
        crate::hidden_windows_command(&update.path)
            .args([
                "/VERYSILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/CLOSEAPPLICATIONS",
            ])
            .spawn()
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&update.path)
            .spawn()
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    Err("automatic installation is not available on this platform".to_owned())
}

async fn verify_staged(update: &VerifiedUpdate) -> Result<(), String> {
    let mut file = tokio::fs::File::open(&update.path)
        .await
        .map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        use tokio::io::AsyncReadExt as _;
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        if size > MAX_UPDATE_BYTES {
            return Err("staged update exceeded the size limit".to_owned());
        }
        hasher.update(&buffer[..read]);
    }
    if format!("{:x}", hasher.finalize()) != update.sha256 {
        return Err("staged update changed after it was downloaded".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_parser_accepts_release_format_and_rejects_junk() {
        let hash = "517174b5e978a46e88864a0650c449b4561a094eab7e0b7af2c94bcae13d83c9";
        assert_eq!(parse_sha256(&format!("{hash}  aibo.exe\n")).unwrap(), hash);
        assert!(parse_sha256("not-a-checksum").is_err());
    }

    #[tokio::test]
    async fn staged_bytes_are_reverified_before_execution() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.exe");
        tokio::fs::write(&path, b"verified bytes").await.unwrap();
        let sha256 = format!("{:x}", Sha256::digest(b"verified bytes"));
        let update = VerifiedUpdate {
            version: "test".to_owned(),
            path: path.clone(),
            sha256,
        };
        assert!(verify_staged(&update).await.is_ok());
        tokio::fs::write(path, b"tampered bytes").await.unwrap();
        assert!(verify_staged(&update).await.is_err());
    }
}
