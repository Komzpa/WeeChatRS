use std::{io::Cursor, path::PathBuf};

const UPLOAD_URL: &str = "https://files.interdo.me/script.php";
const DOWNLOAD_BASE: &str = "https://files.interdo.me/f.php";

/// Valid Jirafeau expiry keywords. Anything else falls back to "day".
const VALID_TIMES: &[&str] = &[
    "minute", "hour", "day", "week", "fortnight", "month", "quarter", "year", "none",
];

/// Upload `path` to the file-share service and return the public download URL on success.
pub async fn upload(path: PathBuf, duration: &str) -> Result<String, String> {
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());

    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("Cannot read file: {}", e))?;

    upload_bytes(filename, bytes, duration).await
}

pub async fn upload_bytes(
    filename: impl Into<String>,
    bytes: Vec<u8>,
    duration: &str,
) -> Result<String, String> {
    let filename = filename.into();
    let mime = mime_for(&filename);
    let time = if VALID_TIMES.contains(&duration) { duration } else { "day" };
    let file_part = reqwest::multipart::Part::bytes(bytes)
        .file_name(filename.clone())
        .mime_str(mime)
        .map_err(|e| format!("MIME error: {}", e))?;

    // Field order matches the bash script (time first, then file).
    // HTTP/1.1 only + Connection: close replicates curl's --http1.0 behaviour
    // which some PHP/nginx setups require for correct multipart parsing.
    let form = reqwest::multipart::Form::new()
        .text("time", time.to_string())
        .part("file", file_part);

    let client = reqwest::Client::builder()
        .http1_only()
        .connection_verbose(false)
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let resp = client
        .post(UPLOAD_URL)
        .header("Connection", "close")
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("Upload failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("Server returned {}", resp.status()));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;

    // The server returns up to three lines:
    //   line 0: file code (hash)
    //   line 1: delete code
    //   line 2: key code (only if upload password was used)
    // Error responses start with "Error".
    let first_line = body.lines().next().unwrap_or("").trim();

    if first_line.starts_with("Error") || first_line.is_empty() {
        return Err(format!("Server error: {}", first_line));
    }

    direct_download_url(first_line, &filename)
}

fn direct_download_url(file_code: &str, filename: &str) -> Result<String, String> {
    let mut url = url::Url::parse(&format!("{}?h={}&p=1", DOWNLOAD_BASE, file_code))
        .map_err(|e| format!("Invalid download URL: {}", e))?;
    // The fragment is not sent to Jirafeau, but preserves the extension so
    // chat clients can identify and display image links inline.
    url.set_fragment(Some(&filename));
    Ok(url.to_string())
}

/// Read a desktop clipboard image and encode it as PNG.
///
/// `Ok(None)` means that the clipboard contains text or another non-image
/// format, allowing ordinary Ctrl-V text paste to continue silently.
pub fn clipboard_png() -> Result<Option<Vec<u8>>, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("Clipboard unavailable: {}", e))?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(_) => return Ok(None),
    };
    let rgba = image::RgbaImage::from_raw(
        image.width as u32,
        image.height as u32,
        image.bytes.into_owned(),
    )
    .ok_or_else(|| "Clipboard returned an invalid RGBA image".to_string())?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("Cannot encode clipboard image: {}", e))?;
    Ok(Some(png))
}

pub(crate) fn mime_for(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png"              => "image/png",
        "jpg" | "jpeg"     => "image/jpeg",
        "gif"              => "image/gif",
        "webp"             => "image/webp",
        "svg"              => "image/svg+xml",
        "mp4"              => "video/mp4",
        "webm"             => "video/webm",
        "mp3"              => "audio/mpeg",
        "ogg"              => "audio/ogg",
        "pdf"              => "application/pdf",
        "zip"              => "application/zip",
        "txt" | "log"
            | "md"         => "text/plain",
        _                  => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_link_uses_direct_preview_and_keeps_filename_fragment() {
        let raw = direct_download_url("abc", "screen shot.png").unwrap();
        let url = url::Url::parse(&raw).unwrap();
        assert_eq!(url.query(), Some("h=abc&p=1"));
        assert_eq!(url.fragment(), Some("screen%20shot.png"));
        assert!(crate::ui::app::WeeChatApp::is_image_url(&raw));
    }

    #[test]
    fn common_image_mime_types_are_preserved() {
        assert_eq!(mime_for("photo.PNG"), "image/png");
        assert_eq!(mime_for("photo.jpeg"), "image/jpeg");
        assert_eq!(mime_for("archive.unknown"), "application/octet-stream");
    }
}
