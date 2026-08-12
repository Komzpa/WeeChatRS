use crate::relay::backend::{BackendClient, BackendEvent};
use crate::relay::weechat::{WeeChatClient, WeeChatConfig};
use crate::relay::models::*;
use crate::ui::ansi::{ANSIParser, ANSISection, AnsiStyle};
use crate::ui::theme::AppTheme;
use crate::ui::keybinds::KeybindsMap;
use crate::ui::url_safety::is_safe_public_url;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use futures_util::StreamExt;
use sha2::Digest;
use egui::{FontId, ScrollArea, Label, Key, Visuals, TextStyle, FontFamily, Color32, text::LayoutJob, Margin, Frame, Rounding, Stroke, Vec2, Modifiers, Rect, Painter};
use tokio::sync::mpsc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub(crate) enum ImageState {
    Loading,
    Loaded(egui::TextureHandle),
    Failed,
}

pub(crate) struct LinkPreview {
    pub title: Option<String>,
    pub description: Option<String>,
    pub image_url: Option<String>,
    pub site_name: Option<String>,
}

pub(crate) enum PreviewState {
    Loading,
    Loaded(LinkPreview),
    Failed,
}

pub(crate) enum PreparedFileShare {
    MatrixAttachment {
        buffer_id: String,
        filename: String,
        mime: String,
        bytes: Vec<u8>,
    },
    ExternalLink {
        buffer_id: String,
        url: String,
    },
}

const MATRIX_UPLOAD_MAX_ENCODED_CHUNK: usize = 32 * 1024;
const MATRIX_UPLOAD_RAW_CHUNK: usize = MATRIX_UPLOAD_MAX_ENCODED_CHUNK / 4 * 3;
const MATRIX_UPLOAD_MAX_BYTES: usize = 10 * 1024 * 1024;
const MATRIX_UPLOAD_CONFIRMATION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(90);
const MATRIX_ROOM_UPGRADE_CHAIN_LIMIT: usize = 32;
static MATRIX_UPLOAD_NONCE: AtomicU64 = AtomicU64::new(1);

fn matrix_redact_command(event_id: &str) -> Result<String, String> {
    if !event_id.starts_with('$')
        || event_id.len() < 2
        || event_id.chars().any(char::is_whitespace)
    {
        return Err("This line has no valid Matrix event ID".to_owned());
    }
    Ok(format!("/redact {event_id}"))
}

fn matrix_attachment_echo_matches(
    target_buffer_id: Option<&str>,
    expected_filename: Option<&str>,
    buffer_id: &str,
    media: Option<&MatrixMedia>,
    is_self_msg: bool,
) -> bool {
    is_self_msg
        && target_buffer_id == Some(buffer_id)
        && media.is_some_and(|media| expected_filename == Some(media.name.as_str()))
}

fn matrix_attachment_upload(id: String, filename: &str, mime: &str, bytes: &[u8]) -> Result<Vec<String>, String> {
    if filename.is_empty() || mime.is_empty() || bytes.is_empty() || bytes.len() > MATRIX_UPLOAD_MAX_BYTES {
        return Err("Matrix attachment metadata or size is invalid (maximum 10 MiB)".to_owned());
    }
    let mut commands = vec![format!(
        "/matrix-upload begin {id} {} {mime} {} {:x}",
        URL_SAFE_NO_PAD.encode(filename),
        bytes.len(),
        sha2::Sha256::digest(bytes)
    )];
    for (sequence, chunk) in bytes.chunks(MATRIX_UPLOAD_RAW_CHUNK).enumerate() {
        let encoded = URL_SAFE_NO_PAD.encode(chunk);
        debug_assert!(encoded.len() <= MATRIX_UPLOAD_MAX_ENCODED_CHUNK);
        commands.push(format!("/matrix-upload chunk {id} {sequence} {encoded}"));
    }
    commands.push(format!("/matrix-upload commit {id}"));
    Ok(commands)
}

fn next_matrix_upload_id() -> String {
    let sequence = MATRIX_UPLOAD_NONCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "clip{:x}{sequence:x}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    )
}

async fn prepare_file_share(
    buffer_id: String,
    path: PathBuf,
    is_matrix: bool,
    duration: String,
) -> Result<PreparedFileShare, String> {
    if !is_matrix {
        let url = crate::ui::fileshare::upload(path, &duration).await?;
        return Ok(PreparedFileShare::ExternalLink { buffer_id, url });
    }

    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| format!("Cannot inspect file: {error}"))?;
    if !metadata.is_file() {
        return Err("Matrix attachment must be a regular file".to_owned());
    }
    if metadata.len() == 0 || metadata.len() > MATRIX_UPLOAD_MAX_BYTES as u64 {
        return Err("Matrix attachments must be between 1 byte and 10 MiB".to_owned());
    }
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "Matrix attachment has no file name".to_owned())?;
    let mime = crate::ui::fileshare::mime_for(&filename).to_owned();
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| format!("Cannot read file: {error}"))?;
    if bytes.len() != metadata.len() as usize {
        return Err("Matrix attachment changed while it was being read".to_owned());
    }
    Ok(PreparedFileShare::MatrixAttachment {
        buffer_id,
        filename,
        mime,
        bytes,
    })
}

// --- HTML helpers (used in tokio::spawn, so must be free functions) ---

fn extract_attr_val(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    for q in ['"', '\''] {
        let needle = format!("{}={}", attr, q);
        if let Some(pos) = lower.find(&needle) {
            let start = pos + needle.len();
            if let Some(end) = tag[start..].find(q) {
                let val = tag[start..start + end].trim().to_string();
                if !val.is_empty() { return Some(decode_entities(&val)); }
            }
        }
    }
    None
}

fn extract_og_tag(html: &str, property: &str) -> Option<String> {
    let lower = html.to_lowercase();
    for q in ['"', '\''] {
        let needle = format!("property={}{}{}", q, property, q);
        if let Some(pos) = lower.find(&needle) {
            let tag_start = lower[..pos].rfind('<')?;
            let tag_end = tag_start + lower[tag_start..].find('>')?;
            if let Some(val) = extract_attr_val(&html[tag_start..=tag_end], "content") {
                return Some(val);
            }
        }
    }
    None
}

fn extract_html_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title")?;
    let open_end = lower[start..].find('>')? + start + 1;
    let close = lower[open_end..].find("</title>")? + open_end;
    let text = html[open_end..close].trim().to_string();
    if text.is_empty() { None } else { Some(decode_entities(&text)) }
}

fn extract_meta_description(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    for q in ['"', '\''] {
        let needle = format!("name={}description{}", q, q);
        if let Some(pos) = lower.find(&needle) {
            let tag_start = lower[..pos].rfind('<')?;
            let tag_end = tag_start + lower[tag_start..].find('>')?;
            if let Some(val) = extract_attr_val(&html[tag_start..=tag_end], "content") {
                return Some(val);
            }
        }
    }
    None
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
     .replace("&lt;", "<")
     .replace("&gt;", ">")
     .replace("&quot;", "\"")
     .replace("&#39;", "'")
     .replace("&apos;", "'")
     .replace("&nbsp;", " ")
}

async fn fetch_link_preview(url: String) -> Result<LinkPreview, String> {
    if !is_safe_public_url(&url) {
        return Err("blocked: non-public URL".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("Mozilla/5.0 WeeChatRS/0.1 (link preview)")
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    let ct = resp.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if !ct.contains("text/html") {
        return Err("not html".to_string());
    }

    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let html = String::from_utf8_lossy(&bytes[..bytes.len().min(65536)]);

    let title = extract_og_tag(&html, "og:title").or_else(|| extract_html_title(&html));
    let description = extract_og_tag(&html, "og:description")
        .or_else(|| extract_meta_description(&html));
    let image_url = extract_og_tag(&html, "og:image");
    let site_name = extract_og_tag(&html, "og:site_name")
        .or_else(|| url::Url::parse(&url).ok().and_then(|u| u.host_str().map(String::from)));

    if title.is_none() && description.is_none() {
        return Err("no preview data".to_string());
    }

    Ok(LinkPreview { title, description, image_url, site_name })
}

pub const INITIAL_LINES: usize = 300;
const INITIAL_HISTORY_ROWS: usize = 120;
const IMAGE_CACHE_MAX: usize = 200;
const PREVIEW_CACHE_MAX: usize = 200;
const PREFIX_COL_WIDTHS_MAX: usize = 500;
const MAX_INLINE_IMAGE_WIDTH: f32 = 360.0;
const MAX_INLINE_IMAGE_HEIGHT: f32 = 240.0;
const MAX_EXPANDED_IMAGE_WIDTH: f32 = 900.0;
const MAX_EXPANDED_IMAGE_HEIGHT: f32 = 720.0;
const MAX_INLINE_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_INLINE_IMAGE_DIMENSION: u32 = 8192;
const MAX_INLINE_IMAGE_PIXELS: u64 = 25_000_000;

fn matrix_media_cache_path(mxc_uri: &str) -> PathBuf {
    let cache_root = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".cache"))
        })
        .unwrap_or_else(std::env::temp_dir);
    cache_root
        .join("weechatrs")
        .join("matrix-media")
        .join(URL_SAFE_NO_PAD.encode(mxc_uri))
}

fn quote_weechat_argument(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn is_matrix_media_status_line(message: &str) -> bool {
    // The Matrix plugin reports media-command progress through ordinary buffer
    // lines. They are useful in WeeChat itself, but GUI-owned preview downloads
    // are implementation detail and must never become chat history. Transport
    // failures do not include the destination path, so recognize that variant
    // separately; keep path-bearing user-selected downloads visible.
    if message.contains("matrix: Error downloading media ") {
        return true;
    }

    let is_gui_cache_path = message.contains("weechatrs/matrix-media/")
        || message.contains("weechatrs\\matrix-media\\");
    is_gui_cache_path
        && (message.contains("matrix: Downloading media to")
            || message.contains("matrix: Successfully downloaded media to")
            || message.contains("matrix: Error writing media to")
            || message.contains("matrix: Error creating media directory"))
}

fn begin_matrix_media_load(
    image_cache: &mut HashMap<String, ImageState>,
    pending: &mut HashSet<String>,
    cache_key: &str,
) -> bool {
    if image_cache.contains_key(cache_key) || !pending.insert(cache_key.to_owned()) {
        return false;
    }
    image_cache.insert(cache_key.to_owned(), ImageState::Loading);
    true
}

async fn wait_for_matrix_media(path: &Path) -> Result<Vec<u8>, String> {
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut previous_size = None;
    let mut stable_samples = 0;

    loop {
        if let Ok(metadata) = tokio::fs::metadata(path).await {
            let size = metadata.len();
            if size > MAX_INLINE_IMAGE_BYTES {
                return Err(format!(
                    "Matrix media is too large for an inline preview (maximum {} MiB)",
                    MAX_INLINE_IMAGE_BYTES / 1024 / 1024,
                ));
            }
            if size > 0 && previous_size == Some(size) {
                stable_samples += 1;
                if stable_samples >= 10 {
                    let bytes = tokio::fs::read(path)
                        .await
                        .map_err(|error| error.to_string())?;
                    if matrix_image_bytes_complete(&bytes) {
                        #[cfg(unix)]
                        tokio::fs::set_permissions(
                            path,
                            std::fs::Permissions::from_mode(0o600),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                        return Ok(bytes);
                    }
                    // A download may pause without being finished. Keep
                    // waiting instead of caching a transient decode failure.
                    stable_samples = 0;
                }
            } else {
                stable_samples = 0;
            }
            previous_size = Some(size);
        }

        if tokio::time::Instant::now() >= deadline {
            return Err("Matrix media download did not finish".to_owned());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn fetch_bounded_image(url: &str) -> Result<Vec<u8>, String> {
    let response = reqwest::get(url).await.map_err(|error| error.to_string())?;
    if let Some(length) = response.content_length() {
        if length > MAX_INLINE_IMAGE_BYTES {
            return Err("Image is too large for an inline preview".to_owned());
        }
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        if bytes.len().saturating_add(chunk.len()) > MAX_INLINE_IMAGE_BYTES as usize {
            return Err("Image is too large for an inline preview".to_owned());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn validate_inline_image_dimensions(bytes: &[u8]) -> Result<(), String> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let (width, height) = reader.into_dimensions().map_err(|error| error.to_string())?;
    if !inline_image_dimensions_allowed(width, height) {
        return Err("Image dimensions are too large for an inline preview".to_owned());
    }
    Ok(())
}

fn inline_image_dimensions_allowed(width: u32, height: u32) -> bool {
    width <= MAX_INLINE_IMAGE_DIMENSION
        && height <= MAX_INLINE_IMAGE_DIMENSION
        && u64::from(width).saturating_mul(u64::from(height)) <= MAX_INLINE_IMAGE_PIXELS
}

fn matrix_image_bytes_complete(bytes: &[u8]) -> bool {
    validate_inline_image_dimensions(bytes).is_ok()
        && image::load_from_memory(bytes).is_ok()
}

fn avatar_thumbnail(image: &image::DynamicImage, edge: u32) -> image::RgbaImage {
    let side = image.width().min(image.height());
    let left = (image.width() - side) / 2;
    let top = (image.height() - side) / 2;
    image
        .crop_imm(left, top, side, side)
        .resize_exact(edge, edge, image::imageops::FilterType::Lanczos3)
        .to_rgba8()
}

fn update_prefix_column_width(current: f32, measured: f32, cap: f32) -> (f32, bool) {
    let width = current.max(measured).min(cap);
    (width, width > current)
}

fn responsive_prefix_column_cap(row_width: f32, show_timestamps: bool) -> f32 {
    let fraction = if show_timestamps { 0.40 } else { 0.48 };
    (row_width * fraction).clamp(72.0, 210.0)
}

fn prefix_column_cap(configured_cap_px: f32, row_width: f32, show_timestamps: bool) -> f32 {
    configured_cap_px.min(responsive_prefix_column_cap(row_width, show_timestamps))
}

fn prefix_span_layout() -> egui::Layout {
    egui::Layout::left_to_right(egui::Align::Center)
}

/// Fit an inline preview into the chat column.
///
/// Keep the source dimensions for small images and bound larger images so
/// previews cannot take over either the room timeline or the thread panel.
fn inline_image_preview_size(
    original: Vec2,
    available_width: f32,
    viewport_height: f32,
) -> Vec2 {
    if original.x <= 0.0 || original.y <= 0.0 {
        return original;
    }

    let max_width =
        (available_width - 32.0).clamp(1.0, MAX_INLINE_IMAGE_WIDTH);
    let max_height = viewport_height.min(MAX_INLINE_IMAGE_HEIGHT);
    let scale = (max_width / original.x)
        .min(max_height / original.y)
        .min(1.0);
    original * scale
}

fn inline_image_display_size(
    original: Vec2,
    available_width: f32,
    viewport_height: f32,
    expanded: bool,
) -> Vec2 {
    if !expanded {
        return inline_image_preview_size(original, available_width, viewport_height);
    }
    if original.x <= 0.0 || original.y <= 0.0 {
        return original;
    }
    let max_width = (available_width - 32.0).clamp(1.0, MAX_EXPANDED_IMAGE_WIDTH);
    let max_height = viewport_height.min(MAX_EXPANDED_IMAGE_HEIGHT);
    let scale = (max_width / original.x)
        .min(max_height / original.y)
        .min(1.0);
    original * scale
}

fn primary_click_hits_rect(
    primary_clicked: bool,
    interact_pos: Option<egui::Pos2>,
    rect: egui::Rect,
) -> bool {
    primary_clicked && interact_pos.is_some_and(|pos| rect.contains(pos))
}

/// A thread composer must never silently fall back to the room buffer.
fn clipboard_upload_target(
    thread_composer_focused: bool,
    thread_buffer_id: Option<&str>,
    room_buffer_id: Option<&str>,
) -> Option<String> {
    if thread_composer_focused {
        thread_buffer_id.map(ToOwned::to_owned)
    } else {
        room_buffer_id.map(ToOwned::to_owned)
    }
}

fn buffer_supports_matrix_upload(buffer: Option<&Buffer>) -> bool {
    buffer.is_some_and(|buffer| buffer.plugin == "matrix" && buffer.matrix_upload_v1)
}

fn paste_shortcut_pressed(events: &[egui::Event]) -> bool {
    events.iter().any(|event| {
        matches!(event, egui::Event::Paste(_))
            || matches!(
            event,
            egui::Event::Key {
                key: egui::Key::Paste,
                pressed: true,
                ..
            }
        ) || matches!(
            event,
            egui::Event::Key {
                key: egui::Key::V,
                pressed: true,
                modifiers,
                ..
            } if modifiers.command || modifiers.ctrl
        )
    })
}

fn should_probe_clipboard_image(events: &[egui::Event]) -> bool {
    paste_shortcut_pressed(events)
        && !events
            .iter()
            .any(|event| matches!(event, egui::Event::Paste(text) if !text.is_empty()))
}

fn response_primary_clicked(ui: &egui::Ui, response: &egui::Response) -> bool {
    response.clicked()
        || ui.input(|input| {
            primary_click_hits_rect(
                input.pointer.primary_clicked(),
                input.pointer.interact_pos(),
                response.rect,
            )
        })
}

fn open_url_once(output: &mut egui::PlatformOutput, url: &str) {
    // The row context menu can consume a child widget's normal click, so links
    // also inspect the raw pointer edge. More than one response can observe the
    // same edge; the first matching link must remain the target for this frame.
    if output.open_url.is_none() {
        output.open_url = Some(egui::OpenUrl::new_tab(url));
    }
}

#[cfg(test)]
mod inline_matrix_image_tests {
    use super::{
        avatar_thumbnail, begin_matrix_media_load,
        inline_image_dimensions_allowed,
        inline_image_display_size, inline_image_preview_size, is_matrix_media_status_line,
        matrix_image_bytes_complete, matrix_media_cache_path, open_url_once,
        prefix_column_cap, prefix_span_layout, ImageState, primary_click_hits_rect,
        quote_weechat_argument,
        responsive_prefix_column_cap,
        update_prefix_column_width,
    };
    use egui::Vec2;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn avatar_thumbnail_center_crops_then_lanczos_prefilters() {
        let mut source = image::RgbaImage::new(4, 2);
        for y in 0..2 {
            source.put_pixel(0, y, image::Rgba([255, 0, 0, 255]));
            source.put_pixel(1, y, image::Rgba([0, 255, 0, 255]));
            source.put_pixel(2, y, image::Rgba([0, 255, 0, 255]));
            source.put_pixel(3, y, image::Rgba([0, 0, 255, 255]));
        }
        let thumbnail = avatar_thumbnail(&image::DynamicImage::ImageRgba8(source), 64);
        assert_eq!(thumbnail.dimensions(), (64, 64));
        assert!(thumbnail.pixels().all(|pixel| *pixel == image::Rgba([0, 255, 0, 255])));
    }

    #[test]
    fn prefix_spans_keep_logical_order_and_grow_as_one_column() {
        assert_eq!(prefix_span_layout().main_dir, egui::Direction::LeftToRight);
        assert_eq!(update_prefix_column_width(40.0, 72.0, 100.0), (72.0, true));
        assert_eq!(update_prefix_column_width(72.0, 36.0, 100.0), (72.0, false));
        assert_eq!(update_prefix_column_width(72.0, 120.0, 90.0), (90.0, true));
        assert_eq!(update_prefix_column_width(120.0, 72.0, 90.0), (90.0, false));
        assert_eq!(responsive_prefix_column_cap(452.0, true), 180.8);
        assert_eq!(responsive_prefix_column_cap(452.0, false), 210.0);
        assert_eq!(prefix_column_cap(f32::INFINITY, 1100.0, true), 210.0);
        assert_eq!(prefix_column_cap(96.0, 1100.0, true), 96.0);
    }

    #[test]
    fn portrait_preview_is_capped_by_height() {
        let size = inline_image_preview_size(
            Vec2::new(900.0, 1800.0),
            1200.0,
            900.0,
        );
        assert!((size.x - 120.0).abs() < 0.01);
        assert!((size.y - 240.0).abs() < 0.01);
    }

    #[test]
    fn inline_decode_rejects_excessive_dimensions_and_pixel_counts() {
        assert!(inline_image_dimensions_allowed(4096, 4096));
        assert!(!inline_image_dimensions_allowed(8193, 1));
        assert!(!inline_image_dimensions_allowed(6000, 6000));
    }

    #[test]
    fn stalled_partial_matrix_image_is_not_accepted_as_complete() {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            8,
            8,
            image::Rgba([12, 34, 56, 255]),
        ));
        let mut complete = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut complete),
                image::ImageFormat::Png,
            )
            .expect("encode fixture");
        let partial = &complete[..complete.len() / 2];

        assert!(matrix_image_bytes_complete(&complete));
        assert!(!matrix_image_bytes_complete(partial));
    }

    #[test]
    fn landscape_preview_is_capped_by_width() {
        assert_eq!(
            inline_image_preview_size(
                Vec2::new(2000.0, 1000.0),
                1200.0,
                900.0,
            ),
            Vec2::new(360.0, 180.0),
        );
    }

    #[test]
    fn small_preview_is_not_upscaled() {
        assert_eq!(
            inline_image_preview_size(
                Vec2::new(200.0, 100.0),
                1200.0,
                900.0,
            ),
            Vec2::new(200.0, 100.0),
        );
    }

    #[test]
    fn image_at_the_width_cap_keeps_its_source_size() {
        assert_eq!(
            inline_image_preview_size(
                Vec2::new(360.0, 220.0),
                1600.0,
                1000.0,
            ),
            Vec2::new(360.0, 220.0),
        );
    }

    #[test]
    fn click_expansion_uses_a_larger_but_still_bounded_size() {
        let size = inline_image_display_size(
            Vec2::new(1119.0, 1159.0),
            1200.0,
            900.0,
            true,
        );
        assert!((size.x - 695.151).abs() < 0.01);
        assert_eq!(size.y, 720.0);
    }

    #[test]
    fn intercepted_primary_click_still_hits_child_control() {
        let rect = egui::Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(110.0, 50.0));
        assert!(primary_click_hits_rect(
            true,
            Some(egui::pos2(60.0, 35.0)),
            rect
        ));
        assert!(!primary_click_hits_rect(
            true,
            Some(egui::pos2(120.0, 35.0)),
            rect
        ));
        assert!(!primary_click_hits_rect(
            false,
            Some(egui::pos2(60.0, 35.0)),
            rect
        ));
    }

    #[test]
    fn one_pointer_click_cannot_be_retargeted_by_a_later_link() {
        let mut output = egui::PlatformOutput::default();
        open_url_once(
            &mut output,
            "https://trac.osgeo.org/postgis/ticket/5796",
        );
        open_url_once(
            &mut output,
            "https://trac.osgeo.org/postgis/ticket/2362",
        );

        assert_eq!(
            output.open_url.as_ref().map(|target| target.url.as_str()),
            Some("https://trac.osgeo.org/postgis/ticket/5796")
        );
    }

    #[test]
    fn matrix_media_cache_name_is_path_safe() {
        let path = matrix_media_cache_path("mxc://matrix.org/some-media-id");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("bXhjOi8vbWF0cml4Lm9yZy9zb21lLW1lZGlhLWlk")
        );
    }

    #[test]
    fn matrix_media_download_path_is_one_weechat_argument() {
        assert_eq!(
            quote_weechat_argument("/tmp/a path/\"image\""),
            "\"/tmp/a path/\\\"image\\\"\""
        );
    }

    #[test]
    fn hides_only_weechatrs_matrix_media_status_lines() {
        assert!(is_matrix_media_status_line(
            "matrix: Downloading media to /home/user/.cache/weechatrs/matrix-media/key"
        ));
        assert!(!is_matrix_media_status_line(
            "matrix: Successfully downloaded media to /home/user/downloads/image.png"
        ));
        assert!(is_matrix_media_status_line(
            "matrix: Error writing media to /home/user/.cache/weechatrs/matrix-media/key: Os {\n    code: 17,\n    kind: AlreadyExists,\n    message: \"Файл існує\",\n}"
        ));
        assert!(!is_matrix_media_status_line(
            "matrix: Error writing media to /home/user/downloads/image.png: AlreadyExists"
        ));
        assert!(is_matrix_media_status_line(
            "matrix: Error downloading media Http(Request { request::Error { kind: Decode, source: hyper::Error(Body, Custom { kind: UnexpectedEof, error: IncompleteBody }) } })"
        ));
        assert!(is_matrix_media_status_line(
            r"matrix: Error creating media directory C:\Users\user\AppData\Local\weechatrs\matrix-media\key: AlreadyExists"
        ));
        assert!(!is_matrix_media_status_line(
            "matrix: an ordinary room message mentioning Error downloading media"
        ));
    }

    #[test]
    fn matrix_media_request_survives_image_cache_eviction() {
        let key = "mxc://matrix.org/avatar";
        let mut image_cache = HashMap::new();
        let mut pending = HashSet::new();

        assert!(begin_matrix_media_load(&mut image_cache, &mut pending, key));
        assert!(matches!(image_cache.get(key), Some(ImageState::Loading)));

        // The bounded texture cache may evict a loading placeholder. The
        // independent request set must still prevent a second relay download
        // from targeting the same create-new cache path.
        image_cache.remove(key);
        assert!(!begin_matrix_media_load(&mut image_cache, &mut pending, key));

        pending.remove(key);
        assert!(begin_matrix_media_load(&mut image_cache, &mut pending, key));
    }
}

/// Drop entries from `map` until its size is at most `cap`. We don't track
/// insertion order, so eviction picks an arbitrary key — acceptable for caches
/// where the goal is to prevent unbounded growth, not optimal hit rate.
fn cap_map<V>(map: &mut HashMap<String, V>, cap: usize) {
    while map.len() > cap {
        let victim = match map.keys().next() {
            Some(k) => k.clone(),
            None => break,
        };
        map.remove(&victim);
    }
}
pub const LOAD_MORE_LINES: usize = 300;
pub const MAX_STORED_LINES: usize = 10_000;

pub(crate) fn matrix_history_snapshot_count() -> usize {
    // Matrix history accumulates in the WeeChat buffer independently of this
    // GUI. A fixed raw-line increment can reveal no older chat at all when
    // status/error lines are dense, making a successful page look like a
    // no-op. One user request should expose all history WeeChat already
    // retains; the 10k client cap remains the memory and rendering bound.
    MAX_STORED_LINES
}

fn next_history_request_count(current: usize, previous_request: usize) -> Option<usize> {
    let base = current.max(previous_request).max(INITIAL_LINES);
    (base < MAX_STORED_LINES).then(|| (base + LOAD_MORE_LINES).min(MAX_STORED_LINES))
}

pub(crate) fn history_snapshot_is_exhausted(received: usize, requested: usize) -> bool {
    received < requested || received >= MAX_STORED_LINES
}

fn should_auto_request_history(current: usize, attempted: bool, rearmed: bool) -> bool {
    current > 0 && ((current < INITIAL_HISTORY_ROWS && !attempted) || rearmed)
}

#[cfg(test)]
mod scrollback_tests {
    use super::{
        history_snapshot_is_exhausted, matrix_history_snapshot_count,
        next_history_request_count, INITIAL_LINES, LOAD_MORE_LINES, MAX_STORED_LINES,
    };

    #[test]
    fn one_matrix_page_reveals_all_history_already_retained_by_weechat() {
        assert_eq!(matrix_history_snapshot_count(), MAX_STORED_LINES);
    }

    #[test]
    fn expanding_weechat_snapshot_requests_one_older_page() {
        assert_eq!(
            next_history_request_count(INITIAL_LINES, INITIAL_LINES),
            Some(INITIAL_LINES + LOAD_MORE_LINES)
        );
        assert_eq!(
            next_history_request_count(INITIAL_LINES + 50, INITIAL_LINES + LOAD_MORE_LINES),
            Some(INITIAL_LINES + LOAD_MORE_LINES * 2)
        );
    }

    #[test]
    fn history_snapshot_stops_on_short_page_or_retention_limit() {
        assert!(!history_snapshot_is_exhausted(600, 600));
        assert!(history_snapshot_is_exhausted(450, 600));
        assert!(history_snapshot_is_exhausted(MAX_STORED_LINES, MAX_STORED_LINES));
        assert_eq!(
            next_history_request_count(MAX_STORED_LINES, MAX_STORED_LINES),
            None
        );
    }

    #[test]
    fn busy_initial_page_does_not_retry_until_rearmed() {
        assert!(super::should_auto_request_history(20, false, false));
        assert!(!super::should_auto_request_history(20, true, false));
        assert!(super::should_auto_request_history(20, true, true));
    }

    #[test]
    fn empty_initial_page_does_not_auto_load_history() {
        assert!(!super::should_auto_request_history(0, false, false));
        assert!(!super::should_auto_request_history(0, true, true));
    }
}
const THREAD_PANEL_DEFAULT_WIDTH: f32 = 380.0;
const THREAD_PANEL_MIN_WIDTH: f32 = 280.0;
const CHAT_PANEL_MIN_WIDTH: f32 = 360.0;
const COMPACT_MESSAGE_ROW_WIDTH: f32 = 460.0;
const PREFIX_MESSAGE_GAP: f32 = 8.0;
const BUFFERS_MIN_AUTO_WIDTH: f32 = 200.0;
const BUFFERS_MAX_AUTO_WIDTH: f32 = 380.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RightPanelKind {
    None,
    Thread,
    Nicklist,
}

#[derive(Clone, Copy, Debug)]
struct ResponsivePanelLayout {
    show_buffers: bool,
    show_right: bool,
    buffers_max_width: f32,
    buffers_constrained: bool,
    right_width: f32,
    right_min_width: f32,
    right_max_width: f32,
    right_constrained: bool,
    central_width: f32,
}

fn responsive_panel_layout(
    viewport_width: f32,
    show_buffers: bool,
    preferred_buffers_width: f32,
    right_kind: RightPanelKind,
    preferred_right_width: f32,
) -> ResponsivePanelLayout {
    let viewport_width = viewport_width.max(1.0);
    // A thread is an explicitly opened second conversation, so on a tiny
    // viewport it may split the window with the main chat. Passive sidebars
    // must instead yield before they crush the chat into an unreadable strip.
    let central_floor = if right_kind == RightPanelKind::Thread {
        CHAT_PANEL_MIN_WIDTH.min(viewport_width * 0.52)
    } else {
        CHAT_PANEL_MIN_WIDTH.min(viewport_width)
    };
    let (right_fraction, natural_right_min) = match right_kind {
        RightPanelKind::None => (0.0, 0.0),
        RightPanelKind::Thread => (0.48, THREAD_PANEL_MIN_WIDTH),
        RightPanelKind::Nicklist => (0.30, 80.0),
    };

    let right_max_without_buffers = if right_kind == RightPanelKind::None {
        0.0
    } else {
        (viewport_width - central_floor)
            .max(1.0)
            .min(viewport_width * right_fraction)
    };
    let right_min_without_buffers = natural_right_min.min(right_max_without_buffers);
    let right_without_buffers = if right_kind == RightPanelKind::None {
        0.0
    } else {
        preferred_right_width.clamp(
            right_min_without_buffers,
            right_max_without_buffers.max(right_min_without_buffers),
        )
    };

    // Shrink the buffer list before shrinking the explicitly opened thread.
    // If even its 80 px compact form does not fit, suppress it temporarily;
    // self.show_buffers remains untouched and restores it after expansion.
    let buffers_budget = (viewport_width - central_floor - right_without_buffers).max(0.0);
    let buffers_max_width = (viewport_width * 0.40).min(buffers_budget);
    let effective_show_buffers = show_buffers && buffers_max_width >= 80.0;
    let buffers_width = if effective_show_buffers {
        preferred_buffers_width.clamp(80.0, buffers_max_width)
    } else {
        0.0
    };

    let available_after_buffers = (viewport_width - buffers_width).max(1.0);
    let central_floor_after_buffers =
        CHAT_PANEL_MIN_WIDTH.min(available_after_buffers * 0.52);
    let right_max_width = if right_kind == RightPanelKind::None {
        0.0
    } else {
        (available_after_buffers - central_floor_after_buffers)
            .max(1.0)
            .min(available_after_buffers * right_fraction)
    };
    let right_min_width = natural_right_min.min(right_max_width);
    let right_width = if right_kind == RightPanelKind::None {
        0.0
    } else {
        preferred_right_width.clamp(
            right_min_width,
            right_max_width.max(right_min_width),
        )
    };

    let central_width = (available_after_buffers - right_width).max(0.0);
    if right_kind == RightPanelKind::Nicklist
        && central_width < COMPACT_MESSAGE_ROW_WIDTH
    {
        // A fragmented sliver of nicks is not useful. Keep the user's
        // show_nicklist preference intact and temporarily give its width back
        // to the chat; the next wider frame recomputes and restores it.
        return responsive_panel_layout(
            viewport_width,
            show_buffers,
            preferred_buffers_width,
            RightPanelKind::None,
            0.0,
        );
    }

    ResponsivePanelLayout {
        show_buffers: effective_show_buffers,
        show_right: right_kind != RightPanelKind::None,
        buffers_max_width,
        buffers_constrained: effective_show_buffers
            && preferred_buffers_width > buffers_max_width,
        right_width,
        right_min_width,
        right_max_width,
        right_constrained: right_kind != RightPanelKind::None
            && preferred_right_width > right_max_width,
        central_width,
    }
}

fn compact_message_row(width: f32) -> bool {
    width < COMPACT_MESSAGE_ROW_WIDTH
}

fn forget_temporary_panel_width(ctx: &egui::Context, panel_id: &'static str, constrained: bool) {
    if constrained {
        ctx.data_mut(|data| {
            data.remove::<egui::containers::panel::PanelState>(egui::Id::new(panel_id));
        });
    }
}

#[cfg(test)]
mod responsive_layout_tests {
    use super::*;

    #[test]
    fn thread_layout_never_starves_the_chat_at_supported_sizes() {
        for viewport in [1366.0, 1024.0, 900.0, 767.0, 640.0, 400.0] {
            let layout = responsive_panel_layout(
                viewport,
                true,
                400.0,
                RightPanelKind::Thread,
                380.0,
            );
            let expected_floor = CHAT_PANEL_MIN_WIDTH.min(viewport * 0.52);
            assert!(
                layout.central_width + 0.5 >= expected_floor,
                "{viewport}px left only {}px for chat",
                layout.central_width
            );
            assert!(
                layout.central_width + layout.right_width
                    + if layout.show_buffers {
                        layout.buffers_max_width.min(400.0)
                    } else {
                        0.0
                    }
                    <= viewport + 0.5
            );
        }
    }

    #[test]
    fn supplied_767px_failure_hides_buffers_instead_of_crushing_messages() {
        let layout = responsive_panel_layout(
            767.0,
            true,
            400.0,
            RightPanelKind::Thread,
            380.0,
        );
        assert!(!layout.show_buffers);
        assert!(layout.central_width >= 398.0);
        assert!(layout.right_width >= 360.0);
    }

    #[test]
    fn narrow_rows_stack_metadata_above_full_width_message_content() {
        assert!(!compact_message_row(640.0));
        assert!(compact_message_row(459.0));
        assert!(compact_message_row(208.0));
    }

    #[test]
    fn nicklist_hides_before_it_forces_the_chat_into_compact_width() {
        for viewport in [400.0, 490.0] {
            let layout = responsive_panel_layout(
                viewport,
                false,
                400.0,
                RightPanelKind::Nicklist,
                180.0,
            );
            assert!(!layout.show_right, "nicklist remained at {viewport}px");
            assert_eq!(layout.right_width, 0.0);
            assert_eq!(layout.central_width, viewport);
        }

        let exact_fit = responsive_panel_layout(
            640.0,
            false,
            400.0,
            RightPanelKind::Nicklist,
            180.0,
        );
        assert!(exact_fit.show_right);
        assert_eq!(exact_fit.central_width, COMPACT_MESSAGE_ROW_WIDTH);
    }

    #[test]
    fn passive_sidebars_yield_to_chat_on_phone_sized_windows() {
        let layout = responsive_panel_layout(
            400.0,
            true,
            400.0,
            RightPanelKind::Nicklist,
            180.0,
        );
        assert!(!layout.show_buffers);
        assert!(!layout.show_right);
        assert_eq!(layout.central_width, 400.0);

        let restored = responsive_panel_layout(
            840.0,
            true,
            240.0,
            RightPanelKind::Nicklist,
            120.0,
        );
        assert!(restored.show_buffers);
        assert!(restored.show_right);
        assert!(restored.central_width >= COMPACT_MESSAGE_ROW_WIDTH);
    }

    #[test]
    fn temporary_side_panel_clamp_does_not_survive_window_expansion() {
        fn render_panel(ctx: &egui::Context, viewport: f32, max_width: f32) -> f32 {
            let mut rendered_width = 0.0;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(viewport, 500.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                let response = egui::SidePanel::right("responsive_restore_test")
                    .resizable(true)
                    .default_width(380.0)
                    .min_width(1.0)
                    .max_width(max_width)
                    .show(ctx, |ui| {
                        ui.set_min_width(ui.available_width());
                    });
                rendered_width = response.response.rect.width();
                forget_temporary_panel_width(
                    ctx,
                    "responsive_restore_test",
                    max_width < 380.0,
                );
                egui::CentralPanel::default().show(ctx, |_| {});
            });
            rendered_width
        }

        let ctx = egui::Context::default();
        let original_width = render_panel(&ctx, 1366.0, 500.0);
        let narrow_width = render_panel(&ctx, 640.0, 307.2);
        assert!(
            narrow_width < original_width - 50.0,
            "panel did not narrow enough: original={original_width}, narrow={narrow_width}"
        );
        assert!(
            egui::containers::panel::PanelState::load(
                &ctx,
                egui::Id::new("responsive_restore_test")
            )
            .is_none()
        );
        let restored_width = render_panel(&ctx, 1366.0, 500.0);
        assert!((restored_width - original_width).abs() < 1.0);
    }
}

#[derive(Clone)]
struct ThreadReplyContext {
    target_event_id: Option<String>,
    sender: Option<String>,
    quotes: Vec<String>,
    has_header: bool,
}

fn reply_quote_text(line: &Line) -> String {
    let quote = line
        .plain_message
        .strip_prefix("> ")
        .unwrap_or(&line.plain_message)
        .trim_start();
    let own_sender = line.plain_prefix.trim();
    if !own_sender.is_empty() {
        if let Some(body) = quote.strip_prefix(own_sender) {
            if let Some(body) = body.strip_prefix(':') {
                return body.trim_start().to_owned();
            }
        }
    }
    quote.to_owned()
}

fn reply_contexts_by_event(lines: &VecDeque<Line>) -> HashMap<String, ThreadReplyContext> {
    let mut replies = HashMap::new();
    for line in lines {
        let (Some(event_id), Some(reply)) = (&line.matrix_event_id, &line.matrix_reply) else {
            continue;
        };
        let context = replies
            .entry(event_id.clone())
            .or_insert_with(|| ThreadReplyContext {
                target_event_id: reply.event_id.clone(),
                sender: reply.sender.clone(),
                quotes: Vec::new(),
                has_header: false,
            });
        if context.target_event_id.is_none() {
            context.target_event_id = reply.event_id.clone();
        }
        if context.sender.is_none() {
            context.sender = reply.sender.clone();
        }
        match reply.kind {
            MatrixReplyLineKind::Header => context.has_header = true,
            MatrixReplyLineKind::Quote => context.quotes.push(reply_quote_text(line)),
        }
    }
    replies
}

fn matrix_event_has_visible_body_line(
    lines: &VecDeque<Line>,
    event_id: &str,
) -> bool {
    lines.iter().any(|candidate| {
        candidate.displayed
            && candidate.matrix_event_id.as_deref() == Some(event_id)
            && candidate.matrix_reply.is_none()
    })
}

fn redundant_room_reply_header(lines: &VecDeque<Line>, line: &Line) -> bool {
    matches!(
        line.matrix_reply.as_ref().map(|reply| reply.kind),
        Some(MatrixReplyLineKind::Header)
    ) && line
        .matrix_event_id
        .as_deref()
        .is_some_and(|event_id| matrix_event_has_visible_body_line(lines, event_id))
}

fn room_reply_context_for_body_line<'a>(
    line: &Line,
    reply_contexts: &'a HashMap<String, ThreadReplyContext>,
) -> Option<&'a ThreadReplyContext> {
    if line.matrix_reply.is_some() {
        return None;
    }
    let event_id = line.matrix_event_id.as_deref()?;
    reply_contexts.get(event_id)
}

fn render_reply_context_card(
    ui: &mut egui::Ui,
    reply: &ThreadReplyContext,
    card_bg: Color32,
    accent_color: Color32,
    text_secondary: Color32,
) -> egui::Response {
    let card = Frame::none()
        .fill(card_bg.linear_multiply(0.72))
        .rounding(Rounding::same(5.0))
        .inner_margin(Margin {
            left: 12.0,
            right: 10.0,
            top: 5.0,
            bottom: 6.0,
        })
        .show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(620.0));
            ui.spacing_mut().item_spacing.y = 2.0;
            let raw_sender = reply.sender.as_deref();
            let display_sender = raw_sender
                .map(compact_matrix_sender_label)
                .or_else(|| {
                    reply.target_event_id.as_deref().map(|event_id| {
                        format!("Reply to {}", short_matrix_event_id(event_id))
                    })
                })
                .unwrap_or_else(|| "Reply".to_owned());
            let sender_response = ui.label(
                egui::RichText::new(&display_sender)
                    .strong()
                    .color(accent_color),
            );
            if let Some(raw_sender) = raw_sender {
                if raw_sender != display_sender {
                    sender_response.on_hover_text(raw_sender);
                }
            } else if let Some(event_id) = reply.target_event_id.as_deref() {
                sender_response.on_hover_text(format!("Reply target: {event_id}"));
            }
            for quote in &reply.quotes {
                ui.label(
                    egui::RichText::new(quote)
                        .color(text_secondary)
                        .italics(),
                );
            }
        });
    let rail = egui::Rect::from_min_max(
        card.response.rect.min,
        egui::pos2(card.response.rect.min.x + 3.0, card.response.rect.max.y),
    );
    ui.painter()
        .rect_filled(rail, Rounding::same(3.0), accent_color);
    card.response
}

fn short_matrix_event_id(event_id: &str) -> String {
    const MAX_CHARS: usize = 18;
    let mut chars = event_id.chars();
    let prefix: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

#[derive(Clone)]
struct ThreadMessageContent {
    message: String,
    media: Option<MatrixMedia>,
}

struct MessagePreviewTargets {
    image_urls: Vec<String>,
    preview_urls: Vec<String>,
    matrix_image_key: Option<String>,
    hovered_url: Option<String>,
}

#[derive(Clone)]
struct ThreadMessageBlock {
    timestamp: chrono::DateTime<chrono::Utc>,
    prefix: String,
    content: Vec<ThreadMessageContent>,
    matrix_event_id: Option<String>,
    reply: Option<ThreadReplyContext>,
}

fn append_thread_line(block: &mut ThreadMessageBlock, line: &Line) {
    match line.matrix_reply.as_ref().map(|reply| reply.kind) {
        Some(MatrixReplyLineKind::Header) => {
            let reply = line.matrix_reply.as_ref().expect("reply header");
            block.reply = Some(ThreadReplyContext {
                target_event_id: reply.event_id.clone(),
                sender: reply.sender.clone(),
                quotes: Vec::new(),
                has_header: true,
            });
        }
        Some(MatrixReplyLineKind::Quote) => {
            let reply = line.matrix_reply.as_ref().expect("reply quote");
            block
                .reply
                .get_or_insert_with(|| ThreadReplyContext {
                    target_event_id: reply.event_id.clone(),
                    sender: reply.sender.clone(),
                    quotes: Vec::new(),
                    has_header: false,
                })
                .quotes
                .push(reply_quote_text(line));
        }
        None => block.content.push(ThreadMessageContent {
            message: line.message.clone(),
            media: line.matrix_media.clone(),
        }),
    }
}

fn group_thread_lines(lines: &VecDeque<Line>) -> Vec<ThreadMessageBlock> {
    let mut blocks: Vec<ThreadMessageBlock> = Vec::new();
    for line in lines.iter().filter(|line| line.displayed) {
        let continues_previous = blocks.last().is_some_and(|block| {
            (line.matrix_event_id.is_some()
                && block.matrix_event_id == line.matrix_event_id)
                || (line.matrix_event_id.is_none()
                    && (line.prefix.is_empty()
                        || (block.prefix == line.prefix && block.timestamp == line.timestamp)))
        });
        if continues_previous {
            if let Some(block) = blocks.last_mut() {
                append_thread_line(block, line);
                continue;
            }
        }
        let mut block = ThreadMessageBlock {
            timestamp: line.timestamp,
            prefix: line.prefix.clone(),
            content: Vec::new(),
            matrix_event_id: line.matrix_event_id.clone(),
            reply: None,
        };
        append_thread_line(&mut block, line);
        blocks.push(block);
    }
    blocks
}

/// Keep the last complete thread visible while a buffer refresh is in flight.
fn stable_thread_snapshot(current: Option<&Buffer>, previous: Option<&Buffer>) -> Option<Buffer> {
    let mut snapshot = current.cloned().or_else(|| previous.cloned())?;
    if snapshot.messages.is_empty() {
        if let Some(previous) = previous.filter(|buffer| !buffer.messages.is_empty()) {
            snapshot.messages = previous.messages.clone();
        }
    }
    Some(snapshot)
}

/// WeeChat buffer IDs are process-local. Keep an open Matrix thread attached
/// to its stable room/root identity when the relay backend recreates buffers.
fn live_thread_buffer_id(
    buffers: &[Buffer],
    current_id: Option<&str>,
    snapshot: Option<&Buffer>,
) -> Option<String> {
    if let Some(current_id) = current_id.filter(|id| {
        buffers
            .iter()
            .any(|buffer| buffer.id == *id && buffer.is_matrix_thread())
    }) {
        return Some(current_id.to_owned());
    }

    let snapshot = snapshot?;
    let room_id = snapshot.matrix_room_id.as_deref()?;
    let thread_root = snapshot.matrix_thread_root.as_deref()?;
    let connection_prefix = snapshot.id.split_once('/').map(|(prefix, _)| prefix);

    buffers
        .iter()
        .find(|buffer| {
            buffer.is_matrix_thread()
                && buffer.matrix_room_id.as_deref() == Some(room_id)
                && buffer.matrix_thread_root.as_deref() == Some(thread_root)
                && connection_prefix.is_none_or(|prefix| {
                    buffer.id.split_once('/').map(|(candidate, _)| candidate)
                        == Some(prefix)
                })
        })
        .map(|buffer| buffer.id.clone())
}

/// Service buffers and Matrix thread buffers are implementation details, not a
/// useful chat destination to reopen after restart.
pub(crate) fn is_restorable_chat_buffer(buffer: &Buffer) -> bool {
    !buffer.hidden
        && !buffer.is_matrix_thread()
        && !matches!(buffer.kind.as_str(), "core" | "server")
}

pub(crate) fn preferred_chat_buffer_id(
    buffers: &[Buffer],
    last_chat_buffer_name: Option<&str>,
) -> Option<String> {
    let replaced_room_ids = replaced_matrix_room_ids(buffers);
    last_chat_buffer_name
        .and_then(|name| buffers.iter().find(|buffer| {
            is_restorable_chat_buffer(buffer)
                && !buffer
                    .matrix_room_id
                    .as_ref()
                    .is_some_and(|room_id| replaced_room_ids.contains(room_id))
                && buffer.full_name == name
        }))
        .or_else(|| buffers.iter().find(|buffer| {
            is_restorable_chat_buffer(buffer)
                && !buffer
                    .matrix_room_id
                    .as_ref()
                    .is_some_and(|room_id| replaced_room_ids.contains(room_id))
        }))
        .map(|buffer| buffer.id.clone())
}

fn replaced_matrix_room_ids(buffers: &[Buffer]) -> HashSet<String> {
    let matrix_room_ids: Vec<&str> = buffers
        .iter()
        .filter_map(|buffer| {
            (!buffer.is_matrix_thread())
                .then_some(buffer.matrix_room_id.as_deref())
                .flatten()
        })
        .collect();
    let mut replaced = HashSet::new();

    for buffer in buffers.iter().filter(|buffer| !buffer.is_matrix_thread()) {
        if let Some(predecessor_room_id) = buffer.matrix_predecessor_room_id.as_deref() {
            if matrix_room_ids
                .iter()
                .any(|room_id| matrix_room_id_matches(predecessor_room_id, room_id))
            {
                replaced.insert(predecessor_room_id.to_owned());
            }
        }

        if let Some(room_id) = buffer.matrix_room_id.as_deref() {
            if buffer
                .matrix_replacement_room_id
                .as_deref()
                .is_some_and(|replacement| {
                    matrix_room_ids
                        .iter()
                        .any(|room_id| matrix_room_id_matches(replacement, room_id))
                })
            {
                replaced.insert(room_id.to_owned());
            }
        }
    }

    replaced
}

fn matrix_room_id_matches(expected: &str, candidate: &str) -> bool {
    expected == candidate
        || candidate
            .strip_prefix(expected)
            .is_some_and(|suffix| suffix.starts_with(':'))
}

pub(crate) fn replacement_buffer_id(
    buffers: &[Buffer],
    current_buffer_id: &str,
) -> Option<String> {
    let connection = current_buffer_id.split_once('/').map(|(prefix, _)| prefix);
    let mut current_id = current_buffer_id.to_owned();
    let mut latest_id = None;
    let mut seen = HashSet::new();

    for _ in 0..MATRIX_ROOM_UPGRADE_CHAIN_LIMIT {
        if !seen.insert(current_id.clone()) {
            break;
        }

        let Some(current) = buffers.iter().find(|buffer| buffer.id == current_id) else {
            break;
        };

        let replacement_room_id = current
            .matrix_replacement_room_id
            .as_deref()
            .or_else(|| current.matrix_room_id.as_deref().and_then(|current_room_id| {
                buffers
                    .iter()
                    .find(|buffer| {
                        !buffer.is_matrix_thread()
                            && buffer.matrix_predecessor_room_id.as_deref() == Some(current_room_id)
                            && connection.is_none_or(|prefix| {
                                buffer.id.split_once('/').map(|(candidate, _)| candidate)
                                    == Some(prefix)
                            })
                    })
                    .and_then(|buffer| buffer.matrix_room_id.as_deref())
            }));

        let Some(replacement_room_id) = replacement_room_id else {
            break;
        };

        let Some(next_id) = buffers
            .iter()
            .find(|buffer| {
                !buffer.is_matrix_thread()
                    && buffer.matrix_room_id.as_deref().is_some_and(|room_id| {
                        matrix_room_id_matches(replacement_room_id, room_id)
                    })
                    && connection.is_none_or(|prefix| {
                        buffer.id.split_once('/').map(|(candidate, _)| candidate) == Some(prefix)
                    })
            })
            .map(|buffer| buffer.id.clone())
        else {
            break;
        };

        latest_id = Some(next_id.clone());
        current_id = next_id;
    }

    latest_id
}

fn predecessor_buffer_ids(buffers: &[Buffer], current_buffer_id: &str) -> Vec<String> {
    let connection = current_buffer_id.split_once('/').map(|(prefix, _)| prefix);
    let mut ids = Vec::new();
    let mut seen_room_ids = HashSet::new();
    let mut current = buffers.iter().find(|buffer| buffer.id == current_buffer_id);

    while let Some(predecessor_room_id) =
        current.and_then(|buffer| buffer.matrix_predecessor_room_id.as_deref())
    {
        if !seen_room_ids.insert(predecessor_room_id.to_owned()) {
            break;
        }
        let Some(predecessor) = buffers.iter().find(|buffer| {
            !buffer.is_matrix_thread()
                && buffer.matrix_room_id.as_deref() == Some(predecessor_room_id)
                && connection.is_none_or(|prefix| {
                    buffer.id.split_once('/').map(|(candidate, _)| candidate) == Some(prefix)
                })
        }) else {
            break;
        };
        ids.push(predecessor.id.clone());
        current = Some(predecessor);
    }
    ids
}

fn upgrade_history_load_buffer_id(
    buffers: &[Buffer],
    current_buffer_id: &str,
    exhausted_buffer_ids: &HashSet<String>,
) -> Option<String> {
    std::iter::once(current_buffer_id.to_owned())
        .chain(predecessor_buffer_ids(buffers, current_buffer_id))
        .find(|buffer_id| !exhausted_buffer_ids.contains(buffer_id))
}

fn composed_upgrade_history(
    buffers: &[Buffer],
    current_buffer_id: &str,
) -> (VecDeque<Line>, HashSet<String>) {
    let Some(current) = buffers.iter().find(|buffer| buffer.id == current_buffer_id) else {
        return (VecDeque::new(), HashSet::new());
    };
    let predecessor_ids = predecessor_buffer_ids(buffers, current_buffer_id);
    let mut ranked_messages: Vec<(usize, bool, Line)> = predecessor_ids
        .iter()
        .rev()
        .filter_map(|buffer_id| buffers.iter().find(|buffer| buffer.id == *buffer_id))
        .enumerate()
        .flat_map(|(rank, buffer)| {
            buffer.messages.iter().map(move |line| {
                let mut line = line.clone();
                // Relay line ids are only unique inside one physical WeeChat
                // buffer. Keep inherited rows distinct from equally-numbered
                // successor rows while retaining Matrix ids for actions.
                line.id = format!("upgrade-history:{}:{}", buffer.id, line.id);
                (rank, true, line)
            })
        })
        .chain(
            current
                .messages
                .iter()
                .cloned()
                .map(|line| (predecessor_ids.len(), false, line)),
        )
        .collect();

    // An event may be represented by several physical lines (reply header,
    // quote and body), so dedupe whole event sources rather than individual
    // lines. Prefer the newest room in the upgrade chain when the same Matrix
    // event exists on both sides of an upgrade.
    let mut preferred_event_rank: HashMap<String, usize> = HashMap::new();
    for (rank, _, line) in &ranked_messages {
        if let Some(event_id) = &line.matrix_event_id {
            preferred_event_rank
                .entry(event_id.clone())
                .and_modify(|preferred| *preferred = (*preferred).max(*rank))
                .or_insert(*rank);
        }
    }
    ranked_messages.retain(|(rank, _, line)| {
        line.matrix_event_id.as_ref().is_none_or(|event_id| {
            preferred_event_rank.get(event_id) == Some(rank)
        })
    });
    ranked_messages.sort_by(|left, right| {
        left.2
            .timestamp
            .cmp(&right.2.timestamp)
            .then_with(|| left.0.cmp(&right.0))
    });
    let mut inherited_line_ids = HashSet::new();
    let mut messages: Vec<Line> = ranked_messages
        .into_iter()
        .map(|(_, inherited, line)| {
            if inherited {
                inherited_line_ids.insert(line.id.clone());
            }
            line
        })
        .collect();
    if messages.len() > MAX_STORED_LINES {
        messages.drain(0..messages.len() - MAX_STORED_LINES);
    }
    inherited_line_ids.retain(|line_id| messages.iter().any(|line| line.id == *line_id));
    (messages.into(), inherited_line_ids)
}

fn buffer_visible_in_sidebar(
    buffer: &Buffer,
    show_hidden_buffers: bool,
    collapsed_servers: &HashSet<String>,
    replaced_room_ids: &HashSet<String>,
) -> bool {
    let is_replaced_matrix_room = buffer
        .matrix_room_id
        .as_ref()
        .is_some_and(|room_id| replaced_room_ids.contains(room_id));
    // A tombstoned Matrix room is history for its successor, not another
    // current chat.  The generic "show hidden buffers" switch must never put
    // it back among live rooms; its lines are composed into the successor.
    if buffer.is_matrix_thread()
        || is_replaced_matrix_room
        || (buffer.hidden && !show_hidden_buffers)
    {
        return false;
    }
    let is_root = buffer.kind == "server" || buffer.kind == "core";
    is_root || !collapsed_servers.contains(&buffer.server)
}

fn buffer_has_sidebar_avatar(buffer: &Buffer) -> bool {
    buffer.plugin == "matrix" && matches!(buffer.kind.as_str(), "channel" | "private")
}

fn sidebar_avatar_initial(name: &str) -> String {
    name.chars()
        .find(|character| character.is_alphanumeric())
        .map(|character| character.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_owned())
}

fn sidebar_avatar_fallback_color(key: &str) -> Color32 {
    const COLORS: [Color32; 8] = [
        Color32::from_rgb(83, 121, 189),
        Color32::from_rgb(111, 83, 189),
        Color32::from_rgb(176, 81, 143),
        Color32::from_rgb(189, 91, 83),
        Color32::from_rgb(184, 128, 61),
        Color32::from_rgb(91, 151, 79),
        Color32::from_rgb(60, 151, 145),
        Color32::from_rgb(67, 130, 174),
    ];
    let hash = key.bytes().fold(0usize, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(byte as usize)
    });
    COLORS[hash % COLORS.len()]
}

#[cfg(test)]
mod thread_tests {
    use super::*;
    use chrono::Utc;

    fn profile(user_id: &str, display_name: &str, nick: &str) -> MatrixMemberProfile {
        MatrixMemberProfile {
            user_id: user_id.to_owned(),
            display_name: display_name.to_owned(),
            nick: nick.to_owned(),
            membership: "join".to_owned(),
            role: "member".to_owned(),
            power_level: Some(0),
            avatar_mxc: None,
        }
    }

    #[test]
    fn profile_lookup_prefers_exact_room_nick_and_rejects_ambiguous_display_names() {
        let profiles = vec![
            profile("@one:example.org", "Alex", "Alex (@one:example.org)"),
            profile("@two:example.org", "Alex", "Alex (@two:example.org)"),
        ];
        assert_eq!(
            matrix_profile_for_nick(&profiles, "Alex (@two:example.org)")
                .map(|profile| profile.user_id),
            Some("@two:example.org".to_owned())
        );
        assert!(matrix_profile_for_nick(&profiles, "Alex").is_none());
        let ranked = vec![profile("@strk:osgeo.org", "strk 🧭", "strk 🧭")];
        assert_eq!(
            matrix_profile_for_nick(&ranked, "&strk 🧭").map(|profile| profile.user_id),
            Some("@strk:osgeo.org".to_owned())
        );

        let candidates = vec![
            MentionCandidate {
                display_name: "Ada".to_owned(),
                user_id: "@ada:example.org".to_owned(),
            },
            MentionCandidate {
                display_name: "Grace".to_owned(),
                user_id: "@grace:example.org".to_owned(),
            },
        ];
        assert_eq!(
            matrix_user_id_for_nick(&candidates, "Ada"),
            Some("@ada:example.org".to_owned())
        );
    }

    fn nick(name: &str) -> Nick {
        Nick {
            name: name.to_owned(),
            prefix: String::new(),
            color_ansi: String::new(),
            away: false,
        }
    }

    #[test]
    fn matrix_nick_rows_keep_identities_separate_with_short_disambiguation() {
        let clusters = matrix_nick_clusters(&[
            nick("GrayShade (@grayshade:dend.ro)"),
            nick("GrayShade (@irc_libera.chat_grayshade:osgeo.org)"),
            nick("Someone else"),
        ]);

        assert_eq!(clusters.len(), 3);
        assert_eq!(clusters[0].display_name, "GrayShade");
        assert_eq!(clusters[0].members.len(), 1);
        assert_eq!(clusters[0].user_ids, ["@grayshade:dend.ro"]);
        assert_eq!(clusters[1].display_name, "GrayShade");
        assert_eq!(
            clusters[1].user_ids,
            ["@irc_libera.chat_grayshade:osgeo.org"]
        );
        assert_eq!(clusters[2].display_name, "Someone else");
        assert!(clusters[2].user_ids.is_empty());
    }

    #[test]
    fn matrix_nick_clusters_do_not_merge_unproven_same_name_members() {
        let clusters = matrix_nick_clusters(&[nick("Alex"), nick("Alex")]);
        assert_eq!(clusters.len(), 2);
        assert!(clusters.iter().all(|cluster| cluster.members.len() == 1));
    }

    #[test]
    fn matrix_identity_sources_keep_native_and_bridge_accounts_distinct() {
        assert_eq!(matrix_identity_source("@grayshade:dend.ro"), "dend.ro");
        assert_eq!(
            matrix_identity_source("@irc_libera.chat_grayshade:osgeo.org"),
            "osgeo.org"
        );
        assert_eq!(matrix_identity_source("@someone:matrix.example:8448"), "matrix.example:8448");
        assert_eq!(matrix_identity_disambiguator("@grayshade:dend.ro"), "dend");
        assert_eq!(
            matrix_identity_disambiguator("@irc_libera.chat_grayshade:osgeo.org"),
            "osgeo"
        );
    }

    #[test]
    fn matrix_message_prefix_uses_the_same_short_disambiguation() {
        let sections = ANSIParser::parse(
            "\x1b[32mGrayShade\x1b[0m (@grayshade:dend.ro)",
        );
        let compact = compact_matrix_prefix_sections(
            "GrayShade (@grayshade:dend.ro)",
            &sections,
        );
        assert_eq!(
            compact
                .iter()
                .map(|section| section.text.as_str())
                .collect::<String>(),
            "GrayShade ·dend"
        );
        assert_eq!(compact[0].style, sections[0].style);
    }

    #[test]
    fn matrix_irc_bridge_prefix_keeps_nick_and_short_network() {
        let sections = ANSIParser::parse("\x1b[38;5;178mirc_libera.chat_darkblueb\x1b[0m");
        let compact = compact_matrix_prefix_sections(
            "irc_libera.chat_darkblueb",
            &sections,
        );
        assert_eq!(
            compact
                .iter()
                .map(|section| section.text.as_str())
                .collect::<String>(),
            "darkblueb ·libera"
        );
        assert_eq!(compact[0].style, sections[0].style);
        assert_eq!(
            compact_matrix_irc_bridge_prefix("@irc_oftc.chat_someone"),
            Some(("@someone".to_owned(), "oftc".to_owned()))
        );
        assert_eq!(
            compact_matrix_sender_label("irc_libera.chat_darkblueb"),
            "darkblueb ·libera"
        );
        assert_eq!(
            compact_matrix_sender_label("GrayShade (@grayshade:dend.ro)"),
            "GrayShade ·dend"
        );
        assert_eq!(compact_matrix_irc_bridge_prefix("ordinary_nick"), None);
    }

    #[test]
    fn decorated_profile_lookup_never_substitutes_another_identity() {
        let profiles = vec![profile("@other:example.org", "GrayShade", "GrayShade")];
        assert!(matrix_profile_for_nick(
            &profiles,
            "GrayShade (@missing:example.org)"
        )
        .is_none());
    }

    #[test]
    fn message_sender_profile_uses_matrix_identity_and_ranked_prefixes() {
        let profiles = vec![profile("@strk:osgeo.org", "strk 🧭", "strk 🧭")];
        let card = message_sender_profile_card(
            "matrix/room",
            "&strk 🧭",
            "matrix",
            true,
            &profiles,
            &[],
        )
        .expect("ranked Matrix sender should resolve to a room member");

        assert_eq!(card.nick, "strk 🧭");
        assert_eq!(card.matrix_user_id.as_deref(), Some("@strk:osgeo.org"));
        assert_eq!(card.matrix.unwrap().display_name, "strk 🧭");
    }

    #[test]
    fn matrix_profile_query_uses_mxid_not_display_nick() {
        let profiles = vec![profile("@strk:osgeo.org", "strk 🧭", "strk 🧭")];
        let card = message_sender_profile_card(
            "matrix/room",
            "&strk 🧭",
            "matrix",
            true,
            &profiles,
            &[],
        )
        .expect("ranked Matrix sender should resolve to a room member");

        assert_eq!(
            matrix_profile_query_target(&card).as_deref(),
            Some("@strk:osgeo.org")
        );
    }

    #[test]
    fn matrix_profile_query_uses_single_identity_when_profile_is_missing() {
        let card = UserProfileCard {
            buffer_id: "matrix/room".to_owned(),
            nick: "Sandro".to_owned(),
            prefix: String::new(),
            server: "matrix".to_owned(),
            is_matrix: true,
            matrix_user_id: None,
            matrix: None,
            matrix_identities: vec![MatrixProfileIdentity {
                user_id: "@strk:osgeo.org".to_owned(),
                profile: None,
            }],
        };

        assert_eq!(
            matrix_profile_query_target(&card).as_deref(),
            Some("@strk:osgeo.org")
        );
    }

    #[test]
    fn matrix_profile_query_refuses_ambiguous_identities() {
        let card = UserProfileCard {
            buffer_id: "matrix/room".to_owned(),
            nick: "Sandro".to_owned(),
            prefix: String::new(),
            server: "matrix".to_owned(),
            is_matrix: true,
            matrix_user_id: None,
            matrix: None,
            matrix_identities: vec![
                MatrixProfileIdentity {
                    user_id: "@strk:osgeo.org".to_owned(),
                    profile: None,
                },
                MatrixProfileIdentity {
                    user_id: "@strk:matrix.org".to_owned(),
                    profile: None,
                },
            ],
        };

        assert!(matrix_profile_query_target(&card).is_none());
    }

    #[test]
    fn message_sender_profile_requires_a_real_irc_nick() {
        let nicks = vec![Nick {
            name: "Komzpa".to_owned(),
            prefix: "@".to_owned(),
            color_ansi: String::new(),
            away: false,
        }];
        let card = message_sender_profile_card(
            "irc/room",
            "@Komzpa",
            "libera",
            false,
            &[],
            &nicks,
        )
        .expect("ranked IRC sender should resolve to the current nicklist");

        assert_eq!(card.nick, "Komzpa");
        assert_eq!(card.prefix, "@");
        assert!(message_sender_profile_card(
            "irc/room",
            "--",
            "libera",
            false,
            &[],
            &nicks,
        )
        .is_none());
    }

    #[test]
    fn legacy_localhost_relay_is_migrated_and_autoconnected() {
        let mut settings = AppSettings::default();
        settings.host = "localhost".to_owned();
        settings.port = "9000".to_owned();
        settings.use_ssl = false;
        settings.save_password = true;

        let profile = migrate_legacy_profile(&settings).expect("legacy profile");
        assert_eq!(profile.label, "localhost");
        assert_eq!(profile.host, "localhost");
        assert_eq!(profile.port, "9000");
        assert!(!profile.use_ssl);
        assert!(profile.auto_connect);
        assert!(profile.save_password);
    }

    #[test]
    fn legacy_saved_keyring_profile_autoconnects_even_if_checkbox_was_false() {
        let mut settings = AppSettings::default();
        settings.host = "localhost".to_owned();
        settings.port = "9000".to_owned();
        settings.use_ssl = false;
        settings.save_password = false;

        let profile = migrate_legacy_profile(&settings).expect("legacy profile");
        assert_eq!(profile.label, "localhost");
        assert_eq!(profile.host, "localhost");
        assert_eq!(profile.port, "9000");
        assert!(!profile.use_ssl);
        assert!(profile.auto_connect);
        assert!(!profile.save_password);
    }

    #[test]
    fn untouched_defaults_do_not_create_a_phantom_connection() {
        assert!(migrate_legacy_profile(&AppSettings::default()).is_none());
    }

    fn line(id: &str, prefix: &str, message: &str, event_id: &str) -> Line {
        let mut line = Line::new(
            id.to_owned(),
            Utc::now(),
            prefix.to_owned(),
            message.to_owned(),
            true,
            false,
        );
        line.matrix_event_id = Some(event_id.to_owned());
        line
    }

    #[test]
    fn groups_multiline_matrix_event_into_one_thread_message() {
        let lines = VecDeque::from([
            line("1", "alice", "root", "$root:example.org"),
            line("2", "bot", "first line", "$reply:example.org"),
            line("3", "bot", "", "$reply:example.org"),
            line("4", "bot", "last line", "$reply:example.org"),
        ]);
        let blocks = group_thread_lines(&lines);
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            blocks[1]
                .content
                .iter()
                .map(|content| content.message.as_str())
                .collect::<Vec<_>>(),
            ["first line", "", "last line"]
        );
    }

    #[test]
    fn separates_reply_context_from_thread_message_body() {
        let mut header = line(
            "1",
            "bob",
            "Reply to Alice:",
            "$reply:example.org",
        );
        header.matrix_reply = Some(MatrixReplyContext {
            event_id: Some("$original:example.org".to_owned()),
            sender: Some("Alice".to_owned()),
            kind: MatrixReplyLineKind::Header,
        });
        let mut quote = line(
            "2",
            "bob",
            "> original message",
            "$reply:example.org",
        );
        quote.matrix_reply = Some(MatrixReplyContext {
            event_id: Some("$original:example.org".to_owned()),
            sender: Some("Alice".to_owned()),
            kind: MatrixReplyLineKind::Quote,
        });
        let body = line("3", "bob", "new message", "$reply:example.org");

        let blocks =
            group_thread_lines(&VecDeque::from([header, quote, body]));

        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0]
                .content
                .iter()
                .map(|content| content.message.as_str())
                .collect::<Vec<_>>(),
            ["new message"]
        );
        let reply = blocks[0].reply.as_ref().expect("reply context");
        assert_eq!(
            reply.target_event_id.as_deref(),
            Some("$original:example.org")
        );
        assert_eq!(reply.sender.as_deref(), Some("Alice"));
        assert_eq!(reply.quotes, ["original message"]);
    }

    #[test]
    fn combines_room_reply_header_and_quote_without_repeating_current_sender() {
        let mut header = line(
            "1",
            "lbart[m]",
            "Reply to Regina Obe:",
            "$reply:example.org",
        );
        header.matrix_reply = Some(MatrixReplyContext {
            event_id: Some("$original:example.org".to_owned()),
            sender: Some("Regina Obe".to_owned()),
            kind: MatrixReplyLineKind::Header,
        });
        let mut quote = line(
            "2",
            "lbart[m]",
            "> lbart[m]:  oslandia still manages this",
            "$reply:example.org",
        );
        quote.matrix_reply = Some(MatrixReplyContext {
            event_id: Some("$original:example.org".to_owned()),
            sender: Some("Regina Obe".to_owned()),
            kind: MatrixReplyLineKind::Quote,
        });

        let contexts = reply_contexts_by_event(&VecDeque::from([header, quote]));
        let context = contexts
            .get("$reply:example.org")
            .expect("one combined reply context");
        assert!(context.has_header);
        assert_eq!(
            context.target_event_id.as_deref(),
            Some("$original:example.org")
        );
        assert_eq!(context.sender.as_deref(), Some("Regina Obe"));
        assert_eq!(context.quotes, ["oslandia still manages this"]);
    }

    #[test]
    fn keeps_orphaned_reply_quote_visible_at_a_history_boundary() {
        let mut quote = line(
            "2",
            "lbart[m]",
            "> older message at the pagination boundary",
            "$reply:example.org",
        );
        quote.matrix_reply = Some(MatrixReplyContext {
            event_id: Some("$original:example.org".to_owned()),
            sender: Some("Regina Obe".to_owned()),
            kind: MatrixReplyLineKind::Quote,
        });

        let contexts = reply_contexts_by_event(&VecDeque::from([quote]));
        let context = contexts
            .get("$reply:example.org")
            .expect("orphaned quote context");
        assert!(!context.has_header);
        assert_eq!(context.quotes, ["older message at the pagination boundary"]);
    }

    #[test]
    fn room_reply_header_attaches_to_following_body_line() {
        let mut header = line(
            "1",
            "bob",
            "Reply to $original:example.org",
            "$reply:example.org",
        );
        header.matrix_reply = Some(MatrixReplyContext {
            event_id: Some("$original:example.org".to_owned()),
            sender: None,
            kind: MatrixReplyLineKind::Header,
        });
        let body = line("2", "bob", "new message", "$reply:example.org");
        let lines = VecDeque::from([header.clone(), body.clone()]);

        assert!(redundant_room_reply_header(&lines, &header));
        let contexts = reply_contexts_by_event(&lines);
        let reply = room_reply_context_for_body_line(&body, &contexts)
            .expect("attached reply context");
        assert_eq!(
            reply.target_event_id.as_deref(),
            Some("$original:example.org")
        );
        assert_eq!(reply.sender.as_deref(), None);
        assert!(reply.has_header);
    }

    #[test]
    fn orphaned_room_reply_header_stays_visible_without_body_line() {
        let mut header = line(
            "1",
            "bob",
            "Reply to $original:example.org",
            "$reply:example.org",
        );
        header.matrix_reply = Some(MatrixReplyContext {
            event_id: Some("$original:example.org".to_owned()),
            sender: None,
            kind: MatrixReplyLineKind::Header,
        });
        let lines = VecDeque::from([header.clone()]);

        assert!(!redundant_room_reply_header(&lines, &header));
    }

    #[test]
    fn grouping_preserves_matrix_media_with_its_message() {
        let mut media_line = line("1", "alice", "image", "$image:example.org");
        media_line.matrix_media = Some(MatrixMedia {
            mxc_uri: "mxc://example.org/screenshot".to_owned(),
            name: "screenshot.png".to_owned(),
            kind: "image".to_owned(),
        });

        let blocks = group_thread_lines(&VecDeque::from([media_line]));

        let media = blocks[0].content[0]
            .media
            .as_ref()
            .expect("grouped Matrix media");
        assert_eq!(media.mxc_uri, "mxc://example.org/screenshot");
        assert_eq!(media.name, "screenshot.png");
        assert_eq!(media.kind, "image");
    }

    #[test]
    fn clipboard_target_never_falls_back_from_thread_to_room() {
        assert_eq!(
            clipboard_upload_target(true, Some("thread"), Some("room")),
            Some("thread".to_owned())
        );
        assert_eq!(clipboard_upload_target(true, None, Some("room")), None);
        assert_eq!(
            clipboard_upload_target(false, None, Some("room")),
            Some("room".to_owned())
        );
    }

    #[test]
    fn captured_clipboard_target_survives_room_change() {
        let upload_target = clipboard_upload_target(true, Some("thread"), Some("room-a"));
        let current_room_after_upload = "room-b";

        assert_eq!(current_room_after_upload, "room-b");
        assert_eq!(upload_target, Some("thread".to_owned()));
    }

    #[test]
    fn clipboard_image_probe_does_not_compete_with_text_paste() {
        let mut modifiers = egui::Modifiers::default();
        modifiers.ctrl = true;
        modifiers.command = true;
        let paste_key = egui::Event::Key {
            key: egui::Key::V,
            physical_key: Some(egui::Key::V),
            pressed: true,
            repeat: false,
            modifiers,
        };

        assert!(paste_shortcut_pressed(&[paste_key.clone()]));
        assert!(should_probe_clipboard_image(&[paste_key.clone()]));
        assert!(!should_probe_clipboard_image(&[
            egui::Event::Paste("hello".to_owned()),
            paste_key,
        ]));
        assert!(paste_shortcut_pressed(&[egui::Event::Paste(String::new())]));
        assert!(should_probe_clipboard_image(&[egui::Event::Paste(
            String::new(),
        )]));
        assert!(!should_probe_clipboard_image(&[]));
    }

    #[test]
    fn matrix_clipboard_upload_keeps_chunk_order_and_round_trips_bytes() {
        let bytes = (0..(MATRIX_UPLOAD_RAW_CHUNK + 17))
            .map(|n| (n % 251) as u8)
            .collect::<Vec<_>>();
        let commands = matrix_attachment_upload("cliptest".to_owned(), "sample.bin", "application/octet-stream", &bytes).unwrap();
        assert!(commands[0].starts_with("/matrix-upload begin cliptest "));
        assert!(commands[0].contains(" application/octet-stream "));
        assert!(!commands[0].contains("http"));
        let mut reconstructed = Vec::new();
        for (sequence, command) in commands[1..commands.len() - 1].iter().enumerate() {
            let fields = command.split_ascii_whitespace().collect::<Vec<_>>();
            assert_eq!(fields[0..3], ["/matrix-upload", "chunk", "cliptest"]);
            assert_eq!(fields[3], sequence.to_string());
            assert!(fields[4].len() <= MATRIX_UPLOAD_MAX_ENCODED_CHUNK);
            reconstructed.extend(URL_SAFE_NO_PAD.decode(fields[4]).unwrap());
        }
        assert_eq!(reconstructed, bytes);
        assert_eq!(commands.last().unwrap(), "/matrix-upload commit cliptest");
    }

    #[test]
    fn matrix_clipboard_upload_rejects_oversized_payload_before_queuing_chunks() {
        let bytes = vec![0; MATRIX_UPLOAD_MAX_BYTES + 1];
        assert!(matrix_attachment_upload("cliptest".to_owned(), "huge.bin", "application/octet-stream", &bytes).is_err());
    }

    #[test]
    fn matrix_upload_requires_an_explicit_backend_capability() {
        let mut buffer = thread_buffer("thread");
        assert!(buffer_supports_matrix_upload(Some(&buffer)));

        buffer.matrix_upload_v1 = false;
        assert!(!buffer_supports_matrix_upload(Some(&buffer)));

        buffer.matrix_upload_v1 = true;
        buffer.plugin = "irc".to_owned();
        assert!(!buffer_supports_matrix_upload(Some(&buffer)));
        assert!(!buffer_supports_matrix_upload(None));
    }

    #[test]
    fn matrix_upload_stays_pending_until_exact_own_media_echo() {
        let media = MatrixMedia {
            mxc_uri: "mxc://example.org/upload".to_owned(),
            name: "clipboard.png".to_owned(),
            kind: "image".to_owned(),
        };
        assert!(matrix_attachment_echo_matches(
            Some("matrix/thread"),
            Some("clipboard.png"),
            "matrix/thread",
            Some(&media),
            true,
        ));
        assert!(!matrix_attachment_echo_matches(
            Some("matrix/thread"),
            Some("clipboard.png"),
            "matrix/room",
            Some(&media),
            true,
        ));
        assert!(!matrix_attachment_echo_matches(
            Some("matrix/thread"),
            Some("clipboard.png"),
            "matrix/thread",
            Some(&media),
            false,
        ));
    }

    #[test]
    fn delete_message_uses_only_an_exact_matrix_event_id() {
        assert_eq!(
            matrix_redact_command("$event:example.org").unwrap(),
            "/redact $event:example.org",
        );
        assert!(matrix_redact_command("latest").is_err());
        assert!(matrix_redact_command("$event:example.org reason").is_err());
    }

    #[tokio::test]
    async fn matrix_file_share_prepares_native_bytes_without_external_url() {
        let path = std::env::temp_dir().join(format!(
            "weechatrs-matrix-attachment-{}.pdf",
            next_matrix_upload_id()
        ));
        let bytes = b"%PDF-1.7\nnative Matrix attachment";
        std::fs::write(&path, bytes).unwrap();

        let prepared = prepare_file_share(
            "matrix/thread".to_owned(),
            path.clone(),
            true,
            "day".to_owned(),
        )
        .await
        .unwrap();
        std::fs::remove_file(path).unwrap();

        match prepared {
            PreparedFileShare::MatrixAttachment {
                buffer_id,
                filename,
                mime,
                bytes: actual,
            } => {
                assert_eq!(buffer_id, "matrix/thread");
                assert!(filename.ends_with(".pdf"));
                assert_eq!(mime, "application/pdf");
                assert_eq!(actual, bytes);
            }
            PreparedFileShare::ExternalLink { .. } => {
                panic!("Matrix file was routed to the external file host")
            }
        }
    }

    #[test]
    fn transient_empty_refresh_keeps_visible_thread_messages() {
        let mut previous = thread_buffer("thread");
        previous
            .messages
            .push_back(line("1", "alice", "existing message", "$event:example.org"));
        let current = thread_buffer("thread");

        let snapshot = stable_thread_snapshot(Some(&current), Some(&previous))
            .expect("stable thread snapshot");

        assert_eq!(snapshot.messages.len(), 1);
        assert_eq!(snapshot.messages[0].message, "existing message");
        assert_eq!(
            stable_thread_snapshot(None, Some(&previous))
                .expect("missing refresh keeps previous snapshot")
                .messages
                .len(),
            1
        );
    }

    #[test]
    fn open_thread_remaps_after_backend_recreates_buffer_ids() {
        let old = thread_buffer("local/1785554655975327");
        let mut recreated = thread_buffer("local/1785557588310747");
        recreated.messages.push_back(line(
            "1",
            "alice",
            "restored message",
            "$event:example.org",
        ));

        let remapped = live_thread_buffer_id(
            &[recreated],
            Some("local/1785554655975327"),
            Some(&old),
        );
        assert_eq!(
            remapped.as_deref(),
            Some("local/1785557588310747"),
        );
        assert_eq!(
            clipboard_upload_target(true, remapped.as_deref(), Some("local/room"))
                .as_deref(),
            Some("local/1785557588310747"),
        );
    }

    fn thread_buffer(id: &str) -> Buffer {
        Buffer {
            id: id.to_owned(),
            number: 1,
            name: "thread".to_owned(),
            full_name: "matrix.thread".to_owned(),
            plugin: "matrix".to_owned(),
            kind: "channel".to_owned(),
            server: "matrix".to_owned(),
            own_nick: "Darafei Praliaskouski".to_owned(),
            messages: VecDeque::new(),
            nicks: Vec::new(),
            mention_candidates: Vec::new(),
            matrix_member_profiles: Vec::new(),
            activity: BufferActivity::None,
            unread_count: 0,
            last_read_id: None,
            last_markread_ts: None,
            topic: String::new(),
            modes: String::new(),
            hidden: true,
            muted: false,
            has_nicklist: false,
            matrix_room_id: Some("!room:example.org".to_owned()),
            matrix_predecessor_room_id: None,
            matrix_replacement_room_id: None,
            matrix_thread_root: Some("$root:example.org".to_owned()),
            matrix_upload_v1: true,
            matrix_avatar_mxc: None,
            visit_start_marker_id: None,
        }
    }

    #[test]
    fn sidebar_avatars_apply_only_to_matrix_chat_rows() {
        let mut matrix_room = thread_buffer("matrix-room");
        matrix_room.kind = "channel".to_owned();
        matrix_room.matrix_thread_root = None;
        assert!(buffer_has_sidebar_avatar(&matrix_room));

        matrix_room.kind = "server".to_owned();
        assert!(!buffer_has_sidebar_avatar(&matrix_room));

        matrix_room.kind = "channel".to_owned();
        matrix_room.plugin = "irc".to_owned();
        assert!(!buffer_has_sidebar_avatar(&matrix_room));
    }

    #[test]
    fn sidebar_avatar_fallback_ignores_matrix_name_sigils() {
        assert_eq!(sidebar_avatar_initial("#PostGIS"), "P");
        assert_eq!(sidebar_avatar_initial("+OSGeo Open Space"), "O");
        assert_eq!(sidebar_avatar_initial("!!!"), "?");
    }
}

fn message_row_shapes(
    rect: Rect,
    attention: bool,
    hovered: bool,
    accent: Color32,
    hover_tint: Color32,
) -> Vec<egui::Shape> {
    let mut shapes = Vec::with_capacity(2);
    let fill = if attention {
        // egui composites in linear colour space, so even a small sRGBA alpha
        // is quite visible on a dark timeline. Keep this far below selection.
        let alpha = if hovered { 3 } else { 1 };
        Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), alpha)
    } else if hovered {
        Color32::from_rgba_unmultiplied(
            hover_tint.r(),
            hover_tint.g(),
            hover_tint.b(),
            1,
        )
    } else {
        Color32::TRANSPARENT
    };
    if fill != Color32::TRANSPARENT {
        shapes.push(egui::Shape::rect_filled(rect, Rounding::same(5.0), fill));
    }
    if attention {
        let bar = Rect::from_min_max(rect.min, egui::pos2(rect.min.x + 3.0, rect.max.y));
        shapes.push(egui::Shape::rect_filled(
            bar,
            Rounding::same(2.0),
            Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 210),
        ));
    }
    shapes
}

#[cfg(test)]
mod message_row_visual_tests {
    use super::*;

    #[test]
    fn attention_is_a_soft_fill_with_a_distinct_accent_bar() {
        let accent = Color32::from_rgb(72, 128, 220);
        let rect = Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(400.0, 42.0));
        let shapes = message_row_shapes(rect, true, false, accent, Color32::WHITE);
        assert_eq!(shapes.len(), 2, "attention needs a fill and a left bar");
        match &shapes[0] {
            egui::Shape::Rect(shape) => {
                assert_eq!(shape.fill, Color32::from_rgba_unmultiplied(72, 128, 220, 1));
                assert_eq!(shape.stroke, Stroke::NONE);
            }
            shape => panic!("unexpected attention background: {shape:?}"),
        }
        match &shapes[1] {
            egui::Shape::Rect(shape) => assert_eq!(shape.rect.width(), 3.0),
            shape => panic!("unexpected attention marker: {shape:?}"),
        }
    }

    #[test]
    fn ordinary_rows_only_get_a_subtle_fill_while_hovered() {
        let rect = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 42.0));
        assert!(message_row_shapes(
            rect,
            false,
            false,
            Color32::BLUE,
            Color32::WHITE,
        )
        .is_empty());
        let shapes = message_row_shapes(rect, false, true, Color32::BLUE, Color32::WHITE);
        match &shapes[0] {
            egui::Shape::Rect(shape) => {
                assert_eq!(shape.fill, Color32::from_rgba_unmultiplied(255, 255, 255, 1));
                assert_eq!(shape.stroke, Stroke::NONE);
            }
            shape => panic!("unexpected hover background: {shape:?}"),
        }
    }
}

/// Reorder `buffers` by moving the dragged item (and its whole server group when it is a server
/// header) to just before `drop_before_id`, or to the end when `drop_before_id` is `None`.
fn apply_drag_reorder(buffers: &mut Vec<Buffer>, drag_id: &str, drop_before_id: Option<&str>) {
    let drag_idx = match buffers.iter().position(|b| b.id == drag_id) {
        Some(i) => i,
        None => return,
    };

    let is_header = buffers[drag_idx].kind == "server" || buffers[drag_idx].kind == "core";
    let server_key = buffers[drag_idx].server.clone();

    // Indices of all buffers that will move (header + its children when moving a header).
    let group_indices: Vec<usize> = if is_header {
        buffers.iter().enumerate()
            .filter(|(_, b)| b.server == server_key)
            .map(|(i, _)| i)
            .collect()
    } else {
        vec![drag_idx]
    };

    // If the drop target is inside the group being moved, do nothing.
    if let Some(tid) = drop_before_id {
        if group_indices.iter().any(|&i| buffers[i].id == tid) {
            return;
        }
    }

    // Remove from highest index first to keep lower indices valid.
    let mut moved: Vec<Buffer> = group_indices.iter().rev()
        .map(|&i| buffers.remove(i))
        .collect();
    moved.reverse();

    let insert_at = match drop_before_id {
        Some(tid) => buffers.iter().position(|b| b.id == tid).unwrap_or(buffers.len()),
        None => buffers.len(),
    };

    for (offset, buf) in moved.into_iter().enumerate() {
        buffers.insert(insert_at + offset, buf);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum BackendType {
    #[default]
    WeeChat,
    Soju,
}

/// Per-connection saved profile (serialised to AppSettings).
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct ConnectionProfile {
    pub label: String,
    pub backend_type: BackendType,
    pub host: String,
    pub port: String,
    pub nick: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub sasl_username: String,
    pub use_ssl: bool,
    pub accept_invalid_certs: bool,
    pub auto_connect: bool,
    #[serde(default)]
    pub save_password: bool,
    #[serde(default)]
    pub channel: String,
    // SSH tunnel
    #[serde(default)]
    pub ssh_enabled: bool,
    #[serde(default)]
    pub ssh_host: String,
    #[serde(default)]
    pub ssh_port: Option<u16>,
    #[serde(default)]
    pub ssh_user: String,
    #[serde(default)]
    pub ssh_save_password: bool,
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,
}


impl ConnectionProfile {
    /// Stable, filesystem-safe prefix derived from label.
    pub fn prefix(&self) -> String {
        if self.label.is_empty() { return "conn".to_string(); }
        self.label.to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
            .collect()
    }
    /// Keyring key for this profile's relay password.
    pub fn keyring_host_key(&self) -> String { format!("{}@{}", self.label, self.host) }
    /// Keyring key for this profile's SSH tunnel password.
    pub fn ssh_keyring_key(&self) -> String { format!("ssh:{}@{}", self.label, self.ssh_host) }
}

impl Default for ConnectionProfile {
    fn default() -> Self {
        Self {
            label: String::new(),
            backend_type: BackendType::WeeChat,
            host: "localhost".to_string(),
            port: "9001".to_string(),
            nick: String::new(),
            username: String::new(),
            sasl_username: String::new(),
            use_ssl: true,
            accept_invalid_certs: false,
            auto_connect: false,
            save_password: false,
            channel: String::new(),
            ssh_enabled: false,
            ssh_host: String::new(),
            ssh_port: None,
            ssh_user: String::new(),
            ssh_save_password: false,
            auto_reconnect: true,
        }
    }
}

/// Per-connection runtime state (NOT serialised).
pub struct ConnectionHandle {
    pub prefix: String,
    pub label: String,
    pub backend_type: BackendType,
    pub client: Box<dyn BackendClient>,
    pub status: String,
    pub is_connecting: bool,
    pub connecting_pending: bool,
    pub auth_error: Option<String>,
    pub auto_reconnect: bool,
    pub connection_log: VecDeque<String>,
    #[allow(dead_code)] // held for Drop — kills the ssh process on disconnect
    pub ssh_tunnel: Option<crate::ui::ssh_tunnel::SshTunnel>,
}

fn spawn_event_forwarder(
    prefix: String,
    mut from_rx: mpsc::UnboundedReceiver<BackendEvent>,
    to_tx: mpsc::UnboundedSender<(String, BackendEvent)>,
) {
    tokio::spawn(async move {
        while let Some(ev) = from_rx.recv().await {
            let _ = to_tx.send((prefix.clone(), ev));
        }
    });
}

#[derive(Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default)]
    pub backend_type: BackendType,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: String,
    #[serde(default)]
    pub irc_nick: String,
    #[serde(default)]
    pub use_ssl: bool,
    pub show_filtered_lines: bool,
    pub colored_nicks: bool,
    pub theme: AppTheme,
    pub font_size: f32,
    pub use_monospace: bool,
    pub show_timestamps: bool,
    pub show_buffers: bool,
    pub show_nicklist: bool,
    pub auto_reconnect: bool,
    pub show_titlebar: bool,
    pub show_server_headers: bool,
    pub show_inline_images: bool,
    pub show_link_previews: bool,
    #[serde(default)]
    pub emoji_rendering: bool,
    pub opacity: f32,
    #[serde(default)]
    pub show_hidden_buffers: bool,
    #[serde(default)]
    pub buffer_order: Vec<String>,
    #[serde(default)]
    pub cleared_buffer_ids: HashSet<String>,
    /// Buffers explicitly read by the user, keyed by their stable prefixed full name.
    /// Relay buffer IDs are process-local and change whenever WeeChat restarts.
    #[serde(default)]
    pub cleared_buffer_names: HashSet<String>,
    #[serde(default)]
    pub read_markers: HashMap<String, SavedReadMarker>,
    #[serde(default)]
    pub save_password: bool,
    #[serde(default)]
    pub font_name: String,
    #[serde(default)]
    pub font_path: String,
    #[serde(default)]
    pub muted_buffer_names: HashSet<String>,
    #[serde(default = "default_true")]
    pub show_toolbar: bool,
    #[serde(default = "default_nicklist_width")]
    pub nicklist_width: f32,
    #[serde(default)]
    pub buffers_width: f32,
    #[serde(default)]
    pub accept_invalid_certs: bool,
    /// Multi-connection profiles (new).
    #[serde(default)]
    pub connections: Vec<ConnectionProfile>,
    /// Max chars in prefix column (0 = auto/dynamic, matches weechat.look.prefix_align_max).
    #[serde(default)]
    pub prefix_align_max: usize,
    /// Separator between prefix column and message (matches weechat.look.prefix_suffix).
    #[serde(default = "default_prefix_suffix")]
    pub prefix_suffix: String,
    /// Duration passed to files.interdo.me upload API (default "24h").
    #[serde(default = "default_file_share_duration")]
    pub file_share_duration: String,
    #[serde(default)]
    pub keybinds: KeybindsMap,
    /// Server group keys (buffer.server) whose child buffers are collapsed in the list.
    #[serde(default)]
    pub collapsed_servers: HashSet<String>,
    /// Derive theme colours from the current desktop wallpaper.
    #[serde(default)]
    pub adaptive_theme: bool,
    /// Stable prefixed full name of the last selected user-visible chat.
    #[serde(default)]
    pub last_chat_buffer_name: Option<String>,
}

fn default_true() -> bool { true }
fn default_nicklist_width() -> f32 { 180.0 }
fn default_prefix_suffix() -> String { "│".to_string() }
fn default_file_share_duration() -> String { "day".to_string() }

fn migrate_legacy_profile(settings: &AppSettings) -> Option<ConnectionProfile> {
    if !settings.connections.is_empty() || settings.host.is_empty() {
        return None;
    }

    let defaults = AppSettings::default();
    let legacy_connection_was_configured = settings.host != defaults.host
        || settings.port != defaults.port
        || settings.use_ssl != defaults.use_ssl
        || settings.accept_invalid_certs != defaults.accept_invalid_certs
        || settings.save_password
        || !settings.irc_nick.is_empty();
    if !legacy_connection_was_configured {
        return None;
    }

    Some(ConnectionProfile {
        label: settings.host.clone(),
        backend_type: settings.backend_type.clone(),
        host: settings.host.clone(),
        port: settings.port.clone(),
        nick: settings.irc_nick.clone(),
        username: String::new(),
        sasl_username: String::new(),
        use_ssl: settings.use_ssl,
        accept_invalid_certs: settings.accept_invalid_certs,
        auto_connect: true,
        save_password: settings.save_password,
        channel: String::new(),
        ssh_enabled: false,
        ssh_host: String::new(),
        ssh_port: None,
        ssh_user: String::new(),
        ssh_save_password: false,
        auto_reconnect: settings.auto_reconnect,
    })
}

pub(crate) fn load_profile_password(profile: &ConnectionProfile) -> Option<String> {
    crate::ui::secure_storage::load_by_key(&profile.keyring_host_key())
        .or_else(|| crate::ui::secure_storage::load(&profile.host, &profile.port))
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: "9001".to_string(),
            use_ssl: true,
            show_filtered_lines: false,
            colored_nicks: true,
            theme: AppTheme::default(),
            font_size: 14.0,
            use_monospace: true,
            show_timestamps: true,
            show_buffers: true,
            show_nicklist: true,
            auto_reconnect: true,
            show_titlebar: true,
            show_server_headers: true,
            show_inline_images: true,
            show_link_previews: true,
            emoji_rendering: false,
            opacity: 1.0,
            show_hidden_buffers: false,
            buffer_order: Vec::new(),
            cleared_buffer_ids: HashSet::new(),
            cleared_buffer_names: HashSet::new(),
            read_markers: HashMap::new(),
            save_password: false,
            font_name: String::new(),
            font_path: String::new(),
            muted_buffer_names: HashSet::new(),
            show_toolbar: true,
            nicklist_width: 0.0,
            buffers_width: 0.0,
            accept_invalid_certs: false,
            backend_type: BackendType::WeeChat,
            irc_nick: String::new(),
            connections: Vec::new(),
            prefix_align_max: 0,
            prefix_suffix: "│".to_string(),
            file_share_duration: "day".to_string(),
            keybinds: KeybindsMap::default(),
            collapsed_servers: HashSet::new(),
            adaptive_theme: false,
            last_chat_buffer_name: None,
        }
    }
}

pub struct WeeChatApp {
    // Multi-connection state
    pub(crate) connections: Vec<ConnectionHandle>,
    pub(crate) profiles: Vec<ConnectionProfile>,
    pub(crate) shared_event_tx: mpsc::UnboundedSender<(String, BackendEvent)>,
    pub(crate) event_rx: mpsc::UnboundedReceiver<(String, BackendEvent)>,

    // Connection management UI state
    pub(crate) show_connection_log: bool,
    pub(crate) connection_log_unread: bool,
    pub(crate) selected_conn_log: Option<String>,
    pub(crate) editing_profile: ConnectionProfile,
    pub(crate) editing_password: String,
    pub(crate) editing_ssh_password: String,
    pub(crate) editing_profile_idx: Option<usize>,
    pub(crate) show_connections: bool,      // connections manager window
    pub(crate) conn_show_add: bool,         // add/edit form inside connections window
    pub(crate) conn_connect_idx: Option<usize>, // index awaiting password before connect
    /// Per-session relay password cache — avoids re-prompting within a session.
    pub(crate) session_passwords: std::collections::HashMap<String, String>,
    /// Per-session SSH password cache.
    pub(crate) session_ssh_passwords: std::collections::HashMap<String, String>,

    pub(crate) buffers: Vec<Buffer>,
    /// id → index into `buffers`. Maintained in lock-step via `rebuild_buffer_idx`
    /// called after every push/retain/extend/sort/clear of `buffers`.
    pub(crate) buffer_idx: HashMap<String, usize>,
    pub(crate) selected_buffer_id: Option<String>,
    pub(crate) last_chat_buffer_name: Option<String>,
    pub(crate) input_text: String,
    // Settings
    pub(crate) show_settings: bool,
    pub(crate) show_filtered_lines: bool,
    pub(crate) colored_nicks: bool,
    pub(crate) theme: AppTheme,
    pub(crate) font_size: f32,
    pub(crate) use_monospace: bool,
    pub(crate) show_timestamps: bool,
    pub(crate) auto_reconnect: bool,
    pub(crate) show_titlebar: bool,
    pub(crate) show_server_headers: bool,
    pub(crate) show_inline_images: bool,
    pub(crate) show_link_previews: bool,
    pub(crate) emoji_rendering: bool,
    pub(crate) opacity: f32,
    pub(crate) show_hidden_buffers: bool,

    // Image preview state
    pub(crate) image_cache: HashMap<String, ImageState>,
    /// Matrix downloads already requested from the relay. This is deliberately
    /// independent from the bounded texture cache: evicting a Loading texture
    /// must not enqueue another create-new write to the same cache path.
    pub(crate) pending_matrix_media: HashSet<String>,
    /// Prefiltered square textures for 22-24 px Matrix sender/nick avatars.
    /// Full-resolution textures stay in `image_cache` for the profile card.
    pub(crate) avatar_texture_cache: HashMap<String, egui::TextureHandle>,
    pub(crate) avatar_image_keys: HashSet<String>,
    pub(crate) image_expanded: HashSet<String>,
    pub(crate) image_full_size: HashSet<String>,
    pub(crate) image_tx: mpsc::UnboundedSender<(String, Result<Vec<u8>, String>)>,
    pub(crate) image_rx: mpsc::UnboundedReceiver<(String, Result<Vec<u8>, String>)>,

    // Link preview state
    pub(crate) preview_cache: HashMap<String, PreviewState>,
    pub(crate) preview_expanded: HashSet<String>,
    pub(crate) preview_tx: mpsc::UnboundedSender<(String, Result<LinkPreview, String>)>,
    pub(crate) preview_rx: mpsc::UnboundedReceiver<(String, Result<LinkPreview, String>)>,

    // UI visibility
    pub(crate) show_buffers: bool,
    pub(crate) show_nicklist: bool,
    pub(crate) show_toolbar: bool,
    pub(crate) nicklist_width: f32,
    pub(crate) buffers_width: f32,

    // Completion state
    pub(crate) completion: Option<CompletionState>,
    pub(crate) mention_completion: Option<MentionCompletionState>,
    pub(crate) selected_mentions: Vec<SelectedMention>,
    pub(crate) command_completion: Option<CommandCompletionState>,
    pub(crate) command_completion_pending: Option<CommandCompletionRequest>,
    pub(crate) command_completion_request_seq: u64,

    // Command History
    pub(crate) command_history: VecDeque<String>,
    pub(crate) history_index: Option<usize>,
    pub(crate) focus_input: bool,
    pub(crate) reply_target: Option<ReplyTarget>,
    pub(crate) profile_card: Option<UserProfileCard>,
    pub(crate) open_thread_buffer_id: Option<String>,
    pub(crate) open_thread_snapshot: Option<Buffer>,
    pub(crate) thread_input_text: String,
    pub(crate) focus_thread_input: bool,
    pub(crate) thread_panel_width: f32,

    // Search state
    pub(crate) show_search: bool,
    pub(crate) search_text: String,

    // Navigation
    pub(crate) pending_buffer_switch: Option<String>,

    // Buffer drag-and-drop reordering
    pub(crate) buffer_order: Vec<String>,
    pub(crate) dragging_buffer_id: Option<String>,
    pub(crate) drag_drop_before_id: Option<String>,

    // Runtime relay IDs for buffers read during this process. Relay IDs are not persisted
    // as authoritative state because WeeChat allocates new ones after every restart.
    pub(crate) cleared_buffer_ids: HashSet<String>,

    // Stable full names for buffers the user has explicitly read. These survive a WeeChat
    // restart and suppress the restored server hotlist until a real new line arrives.
    pub(crate) cleared_buffer_names: HashSet<String>,

    // Last read line per stable connection/buffer name. WeeChat's Relay API does not
    // return the backend read marker, so this is the reload-safe source for the divider.
    pub(crate) read_markers: HashMap<String, SavedReadMarker>,

    // Font selection
    pub(crate) font_name: String,
    pub(crate) font_path: String,
    pub(crate) applied_font_path: String,
    pub(crate) available_fonts: Vec<(String, String)>,

    // Tracks when the current buffer was selected; drives the unread divider transition.
    pub(crate) selected_view_since: Option<std::time::Instant>,

    // Muted buffers (stored by full_name, stable across WeeChat restarts).
    pub(crate) muted_buffer_names: HashSet<String>,
    /// Per-buffer cooldown tracking for OS notifications. Prevents a noisy channel
    /// from spamming the OS notification center.
    pub(crate) last_notif_at: HashMap<String, std::time::Instant>,
    /// Set true when a highlight notification fires while the window isn't focused.
    /// `update()` consumes this and asks the OS to draw user attention to the app
    /// (Dock icon bounce on macOS, taskbar flash on Windows, urgency hint on Linux).
    pub(crate) request_attention: bool,
    /// Deferred notification subsystem init — must run after the OS run loop starts.
    notify_initialized: bool,

    // Set to the buffer ID while a "load more" history request is in flight.
    pub(crate) loading_more_buffer_id: Option<String>,
    /// Largest newest-N snapshot requested from WeeChat for each buffer.
    pub(crate) history_request_counts: HashMap<String, usize>,
    /// Buffers whose backend reported that no older retained history remains.
    pub(crate) history_exhausted_buffer_ids: HashSet<String>,
    /// Old visible row and line count captured before prepending a page.
    pub(crate) history_scroll_anchors: HashMap<String, (String, usize)>,
    /// A top-edge request is armed again only after the user scrolls away.
    pub(crate) history_top_armed_buffer_ids: HashSet<String>,
    /// One-shot viewport acknowledgement for an own message accepted by the relay.
    pub(crate) force_scroll_to_bottom_buffer_id: Option<String>,

    // Transient search text inside the font-family dropdown.
    pub(crate) font_search: String,

    // Prefix column alignment (mirrors weechat.look.prefix_align_max / prefix_suffix).
    pub(crate) prefix_align_max: usize,
    pub(crate) prefix_suffix: String,
    // Per-buffer max prefix pixel width tracked across frames for stable column alignment.
    pub(crate) prefix_col_widths: HashMap<String, f32>,

    // URL captured at right-click time; stays stable while the context menu is open.
    pub(crate) ctx_menu_hovered_url: Option<String>,

    // Keybinds
    pub(crate) keybinds: KeybindsMap,
    /// Action currently being rebound (capture mode).
    pub(crate) editing_keybind: Option<crate::ui::keybinds::KeybindAction>,

    // Collapsed server groups in the buffer list (stored by buffer.server key).
    pub(crate) collapsed_servers: HashSet<String>,

    // Adaptive wallpaper theme
    pub(crate) adaptive_theme: bool,
    /// Wallpaper-derived theme override; None when adaptive is off or not yet loaded.
    pub(crate) adaptive_theme_result: Option<crate::ui::theme::AppTheme>,
    /// Channel from the wallpaper-watcher background thread.
    pub(crate) wallpaper_rx: Option<std::sync::mpsc::Receiver<crate::ui::theme::AppTheme>>,

    // /np (now-playing) channel: tokio task → main loop → send message
    pub(crate) np_tx: mpsc::UnboundedSender<(String, String)>,
    pub(crate) np_rx: mpsc::UnboundedReceiver<(String, String)>,

    // /sysinfo channel: background thread → main loop → send to active buffer
    pub(crate) sysinfo_tx: mpsc::UnboundedSender<(String, String)>,
    pub(crate) sysinfo_rx: mpsc::UnboundedReceiver<(String, String)>,

    // File share: async file preparation/upload → main loop.
    pub(crate) file_share_tx: mpsc::UnboundedSender<Result<PreparedFileShare, String>>,
    pub(crate) file_share_rx: mpsc::UnboundedReceiver<Result<PreparedFileShare, String>>,
    pub(crate) file_share_uploading: bool,
    pub(crate) file_share_status: Option<String>,
    pub(crate) file_share_target_buffer_id: Option<String>,
    pub(crate) file_share_expected_filename: Option<String>,
    pub(crate) file_share_started_at: Option<std::time::Instant>,
    pub(crate) file_share_error: Option<String>,
    pub(crate) file_share_duration: String,
    pub(crate) pending_redaction: Option<RedactionTarget>,
    pub(crate) pending_redaction_error: Option<String>,
}

pub(crate) struct CompletionState {
    pub(crate) original_word: String,
    pub(crate) matches: Vec<String>,
    pub(crate) index: usize,
    pub(crate) word_start_idx: usize,
}

#[derive(Clone)]
pub(crate) struct CommandCompletionState {
    pub(crate) context: String,
    pub(crate) source_text: String,
    pub(crate) cursor_byte_idx: usize,
    pub(crate) base_word: String,
    pub(crate) position_replace: usize,
    pub(crate) add_space: bool,
    pub(crate) matches: Vec<String>,
    pub(crate) index: usize,
}

pub(crate) struct CommandCompletionRequest {
    pub(crate) sequence: u64,
    pub(crate) buffer_id: String,
    pub(crate) input: String,
    pub(crate) cursor_byte_idx: usize,
}

#[derive(Clone)]
pub(crate) struct ReplyTarget {
    pub(crate) buffer_id: String,
    pub(crate) matrix_event_id: String,
    pub(crate) sender: String,
    pub(crate) message: String,
}

#[derive(Clone)]
pub(crate) struct RedactionTarget {
    pub(crate) buffer_id: String,
    pub(crate) matrix_event_id: String,
    pub(crate) sender: String,
    pub(crate) message: String,
}

#[derive(Clone)]
pub(crate) struct UserProfileCard {
    pub(crate) buffer_id: String,
    pub(crate) nick: String,
    pub(crate) prefix: String,
    pub(crate) server: String,
    pub(crate) is_matrix: bool,
    pub(crate) matrix_user_id: Option<String>,
    pub(crate) matrix: Option<MatrixMemberProfile>,
    pub(crate) matrix_identities: Vec<MatrixProfileIdentity>,
}

#[derive(Clone)]
pub(crate) struct MatrixProfileIdentity {
    pub(crate) user_id: String,
    pub(crate) profile: Option<MatrixMemberProfile>,
}

#[derive(Clone)]
struct MatrixNickCluster {
    display_name: String,
    members: Vec<Nick>,
    user_ids: Vec<String>,
}

fn split_disambiguated_matrix_nick(nick: &str) -> Option<(&str, &str)> {
    let nick = nick.strip_suffix(')')?;
    let (display_name, user_id) = nick.rsplit_once(" (")?;
    if display_name.is_empty()
        || !user_id.starts_with('@')
        || !user_id.contains(':')
        || user_id.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some((display_name, user_id))
}

fn matrix_identity_source(user_id: &str) -> String {
    let Some((_, server)) = user_id
        .strip_prefix('@')
        .and_then(|user_id| user_id.split_once(':'))
    else {
        return "Matrix".to_owned();
    };
    server.to_owned()
}

fn matrix_identity_disambiguator(user_id: &str) -> String {
    let source = matrix_identity_source(user_id);
    let host = source.split(':').next().unwrap_or(source.as_str());
    let labels: Vec<_> = host.split('.').filter(|label| !label.is_empty()).collect();
    if labels.len() >= 2 {
        labels[labels.len() - 2].to_owned()
    } else {
        host.to_owned()
    }
}

fn compact_matrix_irc_bridge_prefix(prefix: &str) -> Option<(String, String)> {
    let rank_len: usize = prefix
        .chars()
        .take_while(|ch| matches!(ch, ' ' | '~' | '&' | '@' | '%' | '+'))
        .map(char::len_utf8)
        .sum();
    let (rank, bare_prefix) = prefix.split_at(rank_len);
    let bridge = bare_prefix.strip_prefix("irc_")?;
    let (network, nick) = bridge.split_once(".chat_")?;
    if network.is_empty()
        || nick.is_empty()
        || network.chars().any(char::is_whitespace)
        || nick.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some((format!("{rank}{nick}"), network.to_owned()))
}

fn compact_matrix_sender_label(sender: &str) -> String {
    if let Some((display_name, network)) = compact_matrix_irc_bridge_prefix(sender) {
        return format!("{display_name} ·{network}");
    }
    if let Some((display_name, user_id)) = split_disambiguated_matrix_nick(sender) {
        return format!("{display_name} ·{}", matrix_identity_disambiguator(user_id));
    }
    sender.to_owned()
}

fn compact_matrix_prefix_sections(
    plain_prefix: &str,
    sections: &[ANSISection],
) -> Vec<ANSISection> {
    if let Some((display_name, network)) = compact_matrix_irc_bridge_prefix(plain_prefix) {
        let style = sections
            .iter()
            .find(|section| !section.text.is_empty())
            .map(|section| section.style)
            .unwrap_or_default();
        return vec![
            ANSISection {
                text: display_name,
                style,
                url: None,
            },
            ANSISection {
                text: format!(" ·{network}"),
                style: AnsiStyle::default(),
                url: None,
            },
        ];
    }

    let Some((display_name, user_id)) = split_disambiguated_matrix_nick(plain_prefix) else {
        return sections.to_vec();
    };

    let mut compact = Vec::new();
    let mut remaining = display_name.len();
    for section in sections {
        if remaining == 0 {
            break;
        }
        let take = remaining.min(section.text.len());
        compact.push(ANSISection {
            text: section.text[..take].to_owned(),
            style: section.style,
            url: None,
        });
        remaining -= take;
    }
    compact.push(ANSISection {
        text: format!(" ·{}", matrix_identity_disambiguator(user_id)),
        style: AnsiStyle::default(),
        url: None,
    });
    compact
}

fn matrix_nick_clusters(nicks: &[Nick]) -> Vec<MatrixNickCluster> {
    nicks
        .iter()
        .map(|nick| {
            let (display_name, user_ids) = split_disambiguated_matrix_nick(&nick.name)
                .map(|(display_name, user_id)| {
                    (display_name.to_owned(), vec![user_id.to_owned()])
                })
                .unwrap_or_else(|| (nick.name.clone(), Vec::new()));
            MatrixNickCluster {
                display_name,
                members: vec![nick.clone()],
                user_ids,
            }
        })
        .collect()
}

fn matrix_profile_for_nick(
    profiles: &[MatrixMemberProfile],
    nick: &str,
) -> Option<MatrixMemberProfile> {
    if let Some(profile) = profiles.iter().find(|profile| profile.nick == nick) {
        return Some(profile.clone());
    }
    if let Some((_, user_id)) = split_disambiguated_matrix_nick(nick) {
        return profiles
            .iter()
            .find(|profile| profile.user_id == user_id)
            .cloned();
    }
    // Timeline prefixes include WeeChat rank markers (`&`, `@`, `+`, ...),
    // while member-profile nicks do not. Keep exact matching first so a real
    // display name beginning with one of these characters still wins.
    let nick = nick.trim_start_matches([' ', '~', '&', '@', '%', '+']);
    if let Some(profile) = profiles.iter().find(|profile| profile.nick == nick) {
        return Some(profile.clone());
    }
    let mut matches = profiles
        .iter()
        .filter(|profile| profile.display_name == nick);
    let profile = matches.next()?.clone();
    matches.next().is_none().then_some(profile)
}

fn message_sender_profile_card(
    buffer_id: &str,
    sender: &str,
    server: &str,
    is_matrix: bool,
    matrix_profiles: &[MatrixMemberProfile],
    nicks: &[Nick],
) -> Option<UserProfileCard> {
    if is_matrix {
        let matrix = matrix_profile_for_nick(matrix_profiles, sender)?;
        return Some(UserProfileCard {
            buffer_id: buffer_id.to_owned(),
            nick: matrix.nick.clone(),
            prefix: String::new(),
            server: server.to_owned(),
            is_matrix: true,
            matrix_user_id: Some(matrix.user_id.clone()),
            matrix: Some(matrix),
            matrix_identities: Vec::new(),
        });
    }

    let bare_sender = sender.trim_start_matches([' ', '~', '&', '@', '%', '+']);
    let nick = nicks.iter().find(|nick| nick.name == bare_sender)?;
    Some(UserProfileCard {
        buffer_id: buffer_id.to_owned(),
        nick: nick.name.clone(),
        prefix: nick.prefix.clone(),
        server: server.to_owned(),
        is_matrix: false,
        matrix_user_id: None,
        matrix: None,
        matrix_identities: Vec::new(),
    })
}

fn matrix_profile_query_target(card: &UserProfileCard) -> Option<String> {
    if !card.is_matrix {
        return None;
    }
    card.matrix
        .as_ref()
        .map(|profile| profile.user_id.clone())
        .or_else(|| card.matrix_user_id.clone())
        .or_else(|| {
            (card.matrix_identities.len() == 1)
                .then(|| card.matrix_identities[0].user_id.clone())
        })
}

fn matrix_user_id_for_nick(candidates: &[MentionCandidate], nick: &str) -> Option<String> {
    let mut matches = candidates
        .iter()
        .filter(|candidate| candidate.display_name == nick);
    let user_id = matches.next()?.user_id.clone();
    matches.next().is_none().then_some(user_id)
}

#[derive(Clone)]
pub(crate) struct MentionCompletionState {
    pub(crate) trigger_byte_idx: usize,
    pub(crate) cursor_byte_idx: usize,
    pub(crate) matches: Vec<MentionCandidate>,
    pub(crate) index: usize,
}

#[derive(Clone)]
pub(crate) struct SelectedMention {
    pub(crate) label: String,
    pub(crate) user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedReadMarker {
    pub(crate) line_id: String,
    pub(crate) timestamp_nanos: i64,
}

const BEFORE_FIRST_LOADED_LINE_ID: &str = "__weechatrs_before_first_loaded_line__";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisitMarkerLocation {
    BeforeFirst,
    At(usize),
    Missing,
}

impl VisitMarkerLocation {
    fn is_before(&self, line_index: usize) -> bool {
        match self {
            Self::BeforeFirst => true,
            Self::At(marker_index) => line_index > *marker_index,
            Self::Missing => false,
        }
    }
}

fn visit_marker_location(lines: &VecDeque<Line>, marker_id: &str) -> VisitMarkerLocation {
    if marker_id == BEFORE_FIRST_LOADED_LINE_ID {
        VisitMarkerLocation::BeforeFirst
    } else {
        lines
            .iter()
            .position(|line| line.id == marker_id)
            .map(VisitMarkerLocation::At)
            .unwrap_or(VisitMarkerLocation::Missing)
    }
}

impl SavedReadMarker {
    pub(crate) fn from_line(line: &Line) -> Self {
        Self {
            line_id: line.id.clone(),
            timestamp_nanos: line.timestamp.timestamp_nanos_opt().unwrap_or_default(),
        }
    }

    pub(crate) fn restore_line_id(&self, lines: &VecDeque<Line>) -> Option<String> {
        lines
            .iter()
            .find(|line| line.id == self.line_id)
            .or_else(|| {
                lines
                    .iter()
                    .rev()
                    .find(|line| {
                        line.timestamp.timestamp_nanos_opt().unwrap_or_default()
                            <= self.timestamp_nanos
                    })
            })
            .map(|line| line.id.clone())
    }

    pub(crate) fn restore_visit_line_id(&self, lines: &VecDeque<Line>) -> Option<String> {
        (!lines.is_empty()).then(|| {
            self.restore_line_id(lines)
                .unwrap_or_else(|| BEFORE_FIRST_LOADED_LINE_ID.to_owned())
        })
    }

    pub(crate) fn advanced_with(&self, candidate: Self) -> Self {
        if candidate.timestamp_nanos >= self.timestamp_nanos {
            candidate
        } else {
            self.clone()
        }
    }
}

fn restored_cleared_buffer_names(settings: &AppSettings) -> HashSet<String> {
    let mut names = settings.cleared_buffer_names.clone();
    // Migration for settings written before stable cleared names existed. A saved marker is
    // only created after the user visits a buffer, so it is safe to treat it as read state.
    names.extend(settings.read_markers.keys().cloned());
    names
}

#[cfg(test)]
mod saved_read_marker_tests {
    use super::{
        buffer_visible_in_sidebar, composed_upgrade_history, preferred_chat_buffer_id,
        replaced_matrix_room_ids, replacement_buffer_id, upgrade_history_load_buffer_id,
        visit_marker_location, AppSettings, Buffer, BufferActivity, Line, SavedReadMarker,
        VisitMarkerLocation, BEFORE_FIRST_LOADED_LINE_ID,
    };
    use chrono::{TimeZone, Utc};
    use std::collections::{HashSet, VecDeque};

    fn line(id: &str, timestamp: i64) -> Line {
        Line::new(
            id.to_owned(),
            Utc.timestamp_opt(timestamp, 0).single().unwrap(),
            "alice".to_owned(),
            "message".to_owned(),
            true,
            false,
        )
    }

    fn buffer(id: &str, full_name: &str, kind: &str) -> Buffer {
        Buffer {
            id: id.to_owned(),
            number: 1,
            name: full_name.to_owned(),
            full_name: full_name.to_owned(),
            plugin: "matrix".to_owned(),
            kind: kind.to_owned(),
            server: "matrix".to_owned(),
            own_nick: String::new(),
            messages: VecDeque::new(),
            nicks: Vec::new(),
            mention_candidates: Vec::new(),
            matrix_member_profiles: Vec::new(),
            activity: BufferActivity::None,
            unread_count: 0,
            last_read_id: None,
            last_markread_ts: None,
            topic: String::new(),
            modes: String::new(),
            hidden: false,
            muted: false,
            has_nicklist: true,
            matrix_room_id: None,
            matrix_predecessor_room_id: None,
            matrix_replacement_room_id: None,
            matrix_thread_root: None,
            matrix_upload_v1: false,
            matrix_avatar_mxc: None,
            visit_start_marker_id: None,
        }
    }

    #[test]
    fn saved_read_marker_survives_settings_round_trip() {
        let mut settings = AppSettings::default();
        settings.read_markers.insert(
            "local/matrix.matrix.!room:example.org".to_owned(),
            SavedReadMarker {
                line_id: "123".to_owned(),
                timestamp_nanos: 1_750_000_000_000_000_000,
            },
        );
        settings.cleared_buffer_names.insert(
            "local/matrix.matrix.!room:example.org".to_owned(),
        );
        settings.last_chat_buffer_name =
            Some("local/matrix.matrix.!room:example.org".to_owned());

        let encoded = serde_json::to_string(&settings).unwrap();
        let restored: AppSettings = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored.read_markers, settings.read_markers);
        assert_eq!(
            restored.cleared_buffer_names,
            settings.cleared_buffer_names
        );
        assert_eq!(restored.last_chat_buffer_name, settings.last_chat_buffer_name);
    }

    #[test]
    fn stable_cleared_names_migrate_from_saved_read_markers() {
        let mut settings = AppSettings::default();
        let name = "local/matrix.matrix.!room:example.org".to_owned();
        settings.read_markers.insert(
            name.clone(),
            SavedReadMarker {
                line_id: "old-id".to_owned(),
                timestamp_nanos: 200,
            },
        );

        assert!(super::restored_cleared_buffer_names(&settings).contains(&name));
    }

    #[test]
    fn saved_read_marker_never_moves_backwards() {
        let current = SavedReadMarker {
            line_id: "newer".to_owned(),
            timestamp_nanos: 200,
        };
        let older = SavedReadMarker {
            line_id: "older-snapshot".to_owned(),
            timestamp_nanos: 100,
        };
        let newer = SavedReadMarker {
            line_id: "newest".to_owned(),
            timestamp_nanos: 300,
        };

        assert_eq!(current.advanced_with(older), current);
        assert_eq!(current.advanced_with(newer.clone()), newer);
    }

    #[test]
    fn last_chat_restore_prefers_the_exact_visible_room_over_service_buffers() {
        let core = buffer("local/core", "local/core.weechat", "core");
        let server = buffer("local/server", "local/irc.server.libera", "server");
        let mut thread = buffer("local/thread", "local/matrix.matrix.!room:example.org", "channel");
        thread.matrix_room_id = Some("!room:example.org".to_owned());
        thread.matrix_thread_root = Some("$root:example.org".to_owned());
        let other = buffer("local/other", "local/matrix.matrix.!other:example.org", "channel");
        let remembered = buffer("local/room", "local/matrix.matrix.!room:example.org", "channel");

        assert_eq!(
            preferred_chat_buffer_id(
                &[core, server, thread.clone(), other.clone(), remembered],
                Some("local/matrix.matrix.!room:example.org"),
            ),
            Some("local/room".to_owned()),
        );
        assert_eq!(
            preferred_chat_buffer_id(&[thread, other], None),
            Some("local/other".to_owned()),
        );
    }

    #[test]
    fn matrix_room_upgrade_is_one_visible_chat_with_read_only_predecessor_history() {
        let mut predecessor = buffer(
            "local/old",
            "local/matrix.matrix.!old:example.org",
            "channel",
        );
        predecessor.matrix_room_id = Some("!old:example.org".to_owned());
        predecessor.matrix_replacement_room_id = Some("!new:example.org".to_owned());
        predecessor.messages.push_back(line("old-history", 10));
        predecessor.messages.push_back(line("late-old-message", 40));

        let mut successor = buffer(
            "local/new",
            "local/matrix.matrix.!new:example.org",
            "channel",
        );
        successor.matrix_room_id = Some("!new:example.org".to_owned());
        successor.matrix_predecessor_room_id = Some("!old:example.org".to_owned());
        successor.messages.push_back(line("new-history", 20));
        successor.messages.push_back(line("new-latest", 30));

        let mut thread = buffer(
            "local/new-thread",
            "local/matrix.matrix.!new:example.org.thread.root",
            "channel",
        );
        thread.matrix_room_id = Some("!new:example.org".to_owned());
        thread.matrix_thread_root = Some("$root:example.org".to_owned());

        let buffers = vec![predecessor, thread, successor];
        let replaced = replaced_matrix_room_ids(&buffers);
        assert!(replaced.contains("!old:example.org"));
        assert_eq!(
            replacement_buffer_id(&buffers, "local/old").as_deref(),
            Some("local/new"),
        );
        assert_eq!(
            preferred_chat_buffer_id(
                &buffers,
                Some("local/matrix.matrix.!old:example.org"),
            )
            .as_deref(),
            Some("local/new"),
        );

        let (messages, inherited) = composed_upgrade_history(&buffers, "local/new");
        let ids = messages.iter().map(|line| line.id.as_str()).collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                "upgrade-history:local/old:old-history",
                "new-history",
                "new-latest",
                "upgrade-history:local/old:late-old-message",
            ],
        );
        assert!(inherited.contains("upgrade-history:local/old:old-history"));
        assert!(inherited.contains("upgrade-history:local/old:late-old-message"));
        let mut exhausted = HashSet::new();
        assert_eq!(
            upgrade_history_load_buffer_id(&buffers, "local/new", &exhausted).as_deref(),
            Some("local/new"),
        );
        exhausted.insert("local/new".to_owned());
        assert_eq!(
            upgrade_history_load_buffer_id(&buffers, "local/new", &exhausted).as_deref(),
            Some("local/old"),
        );
    }

    #[test]
    fn matrix_room_upgrade_selection_follows_successor_chain_to_latest_room() {
        let mut oldest = buffer("local/oldest", "oldest", "channel");
        oldest.matrix_room_id = Some("!oldest:example.org".to_owned());
        oldest.matrix_replacement_room_id = Some("!middle:example.org".to_owned());

        let mut middle = buffer("local/middle", "middle", "channel");
        middle.matrix_room_id = Some("!middle:example.org".to_owned());
        middle.matrix_predecessor_room_id = Some("!oldest:example.org".to_owned());
        middle.matrix_replacement_room_id = Some("!latest:example.org".to_owned());

        let mut latest = buffer("local/latest", "latest", "channel");
        latest.matrix_room_id = Some("!latest:example.org".to_owned());
        latest.matrix_predecessor_room_id = Some("!middle:example.org".to_owned());

        let buffers = vec![oldest, middle, latest];
        assert_eq!(
            replacement_buffer_id(&buffers, "local/oldest").as_deref(),
            Some("local/latest"),
        );
        assert_eq!(
            replacement_buffer_id(&buffers, "local/middle").as_deref(),
            Some("local/latest"),
        );
    }

    #[test]
    fn last_chat_restore_falls_back_when_the_saved_room_disappeared() {
        let first_visible = buffer(
            "local/other",
            "local/matrix.matrix.!other:example.org",
            "channel",
        );

        assert_eq!(
            preferred_chat_buffer_id(
                &[first_visible],
                Some("local/matrix.matrix.!gone:example.org"),
            ),
            Some("local/other".to_owned()),
        );
    }

    #[test]
    fn sidebar_visibility_excludes_threads_hidden_rows_and_collapsed_children() {
        let root = buffer("local/root", "server", "server");
        let child = buffer("local/child", "#visible", "channel");
        let mut hidden = buffer("local/hidden", "#hidden", "channel");
        hidden.hidden = true;
        let mut thread = buffer("local/thread", "thread", "channel");
        thread.matrix_room_id = Some("!room:example.org".to_owned());
        thread.matrix_thread_root = Some("$root:example.org".to_owned());

        let collapsed = HashSet::from(["matrix".to_owned()]);
        let replaced = HashSet::new();
        assert!(buffer_visible_in_sidebar(&root, false, &collapsed, &replaced));
        assert!(!buffer_visible_in_sidebar(&child, false, &collapsed, &replaced));
        assert!(!buffer_visible_in_sidebar(&hidden, false, &HashSet::new(), &replaced));
        assert!(buffer_visible_in_sidebar(&hidden, true, &HashSet::new(), &replaced));
        assert!(!buffer_visible_in_sidebar(&thread, true, &HashSet::new(), &replaced));

        let mut predecessor = buffer("local/old", "#postgis", "channel");
        predecessor.matrix_room_id = Some("!old:example.org".to_owned());
        let replaced = HashSet::from(["!old:example.org".to_owned()]);
        assert!(!buffer_visible_in_sidebar(
            &predecessor,
            false,
            &HashSet::new(),
            &replaced,
        ));
        assert!(!buffer_visible_in_sidebar(
            &predecessor,
            true,
            &HashSet::new(),
            &replaced,
        ));
    }

    #[test]
    fn matrix_room_upgrade_hides_predecessor_from_successor_edge_only() {
        let mut predecessor = buffer("local/old", "#postgis", "channel");
        predecessor.matrix_room_id = Some("!old:example.org".to_owned());

        let mut successor = buffer("local/new", "#postgis", "channel");
        successor.matrix_room_id = Some("!new:example.org".to_owned());
        successor.matrix_predecessor_room_id = Some("!old:example.org".to_owned());

        let replaced = replaced_matrix_room_ids(&[predecessor.clone(), successor.clone()]);
        assert!(replaced.contains("!old:example.org"));
        assert_eq!(
            replacement_buffer_id(&[predecessor.clone(), successor.clone()], "local/old")
                .as_deref(),
            Some("local/new"),
        );
        assert!(!buffer_visible_in_sidebar(
            &predecessor,
            true,
            &HashSet::new(),
            &replaced,
        ));
    }

    #[test]
    fn matrix_room_upgrade_handles_legacy_replacement_without_server_name() {
        let mut predecessor = buffer("local/old", "#weechat-matrix", "channel");
        predecessor.matrix_room_id =
            Some("!twcBhHVdZlQWuuxBhN:termina.org.uk".to_owned());
        predecessor.matrix_replacement_room_id =
            Some("!FG5QpOI_8bKulTsRaAaDgVxcRTwg0ZoIRlWHThl69NI".to_owned());

        let mut successor = buffer("local/new", "#weechat-matrix", "channel");
        successor.matrix_room_id =
            Some("!FG5QpOI_8bKulTsRaAaDgVxcRTwg0ZoIRlWHThl69NI".to_owned());
        successor.matrix_predecessor_room_id =
            Some("!twcBhHVdZlQWuuxBhN:termina.org.uk".to_owned());

        let buffers = [predecessor.clone(), successor.clone()];
        let replaced = replaced_matrix_room_ids(&buffers);

        assert!(replaced.contains("!twcBhHVdZlQWuuxBhN:termina.org.uk"));
        assert_eq!(
            replacement_buffer_id(&buffers, "local/old").as_deref(),
            Some("local/new"),
        );
        assert!(!buffer_visible_in_sidebar(
            &predecessor,
            false,
            &HashSet::new(),
            &replaced,
        ));
    }

    #[test]
    fn matrix_room_upgrade_keeps_tombstoned_room_until_successor_is_listed() {
        let mut predecessor = buffer("local/old", "#postgis", "channel");
        predecessor.matrix_room_id = Some("!old:example.org".to_owned());
        predecessor.matrix_replacement_room_id = Some("!new:example.org".to_owned());

        let replaced = replaced_matrix_room_ids(&[predecessor.clone()]);
        assert!(!replaced.contains("!old:example.org"));
        assert!(buffer_visible_in_sidebar(
            &predecessor,
            false,
            &HashSet::new(),
            &replaced,
        ));
        assert!(buffer_visible_in_sidebar(
            &predecessor,
            true,
            &HashSet::new(),
            &replaced,
        ));
    }

    #[test]
    fn upgrade_history_keeps_equal_buffer_local_line_ids_distinct() {
        let mut predecessor = buffer("local/old", "old", "channel");
        predecessor.matrix_room_id = Some("!old:example.org".to_owned());
        predecessor.matrix_replacement_room_id = Some("!new:example.org".to_owned());
        predecessor.messages.push_back(line("7", 10));

        let mut successor = buffer("local/new", "new", "channel");
        successor.matrix_room_id = Some("!new:example.org".to_owned());
        successor.matrix_predecessor_room_id = Some("!old:example.org".to_owned());
        successor.messages.push_back(line("7", 20));

        let (messages, inherited) =
            composed_upgrade_history(&[predecessor, successor], "local/new");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].id, "upgrade-history:local/old:7");
        assert_eq!(messages[1].id, "7");
        assert!(inherited.contains("upgrade-history:local/old:7"));
        assert!(!inherited.contains("7"));
    }

    #[test]
    fn upgrade_history_prefers_all_successor_lines_for_an_overlapping_event() {
        let mut predecessor = buffer("local/old", "old", "channel");
        predecessor.matrix_room_id = Some("!old:example.org".to_owned());
        predecessor.matrix_replacement_room_id = Some("!new:example.org".to_owned());
        let mut old_header = line("old-header", 10);
        old_header.matrix_event_id = Some("$same:example.org".to_owned());
        let mut old_body = line("old-body", 10);
        old_body.matrix_event_id = Some("$same:example.org".to_owned());
        predecessor.messages.extend([old_header, old_body]);

        let mut successor = buffer("local/new", "new", "channel");
        successor.matrix_room_id = Some("!new:example.org".to_owned());
        successor.matrix_predecessor_room_id = Some("!old:example.org".to_owned());
        let mut new_header = line("new-header", 10);
        new_header.matrix_event_id = Some("$same:example.org".to_owned());
        let mut new_body = line("new-body", 10);
        new_body.matrix_event_id = Some("$same:example.org".to_owned());
        successor.messages.extend([new_header, new_body]);

        let (messages, inherited) =
            composed_upgrade_history(&[predecessor, successor], "local/new");
        assert_eq!(
            messages.iter().map(|line| line.id.as_str()).collect::<Vec<_>>(),
            vec!["new-header", "new-body"],
        );
        assert!(inherited.is_empty());
    }

    #[test]
    fn saved_read_marker_restores_after_relay_line_ids_change() {
        let lines = VecDeque::from([
            line("new-id-1", 100),
            line("new-id-2", 200),
            line("new-id-3", 300),
        ]);
        let marker = SavedReadMarker {
            line_id: "old-id-2".to_owned(),
            timestamp_nanos: 200_000_000_000,
        };

        assert_eq!(marker.restore_line_id(&lines).as_deref(), Some("new-id-2"));
    }

    #[test]
    fn saved_marker_before_loaded_page_places_divider_before_first_line() {
        let lines = VecDeque::from([line("301", 301), line("302", 302)]);
        let marker = SavedReadMarker {
            line_id: "1".to_owned(),
            timestamp_nanos: 1_000_000_000,
        };

        assert_eq!(
            marker.restore_visit_line_id(&lines).as_deref(),
            Some(BEFORE_FIRST_LOADED_LINE_ID),
        );
    }

    #[test]
    fn visit_divider_uses_timeline_position_not_numeric_line_id_order() {
        let lines = VecDeque::from([
            line("10", 100),
            line("2", 200),
            line("0", 300),
        ]);

        let marker = visit_marker_location(&lines, "2");
        assert_eq!(marker, VisitMarkerLocation::At(1));
        assert!(!marker.is_before(1));
        assert!(marker.is_before(2));
        assert_eq!(
            visit_marker_location(&lines, BEFORE_FIRST_LOADED_LINE_ID),
            VisitMarkerLocation::BeforeFirst,
        );
    }
}

impl WeeChatApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (shared_event_tx, event_rx) = mpsc::unbounded_channel::<(String, BackendEvent)>();
        let (image_tx, image_rx) = mpsc::unbounded_channel();
        let (preview_tx, preview_rx) = mpsc::unbounded_channel();
        let (np_tx, np_rx) = mpsc::unbounded_channel::<(String, String)>();
        let (sysinfo_tx, sysinfo_rx) = mpsc::unbounded_channel::<(String, String)>();
        let (file_share_tx, file_share_rx) =
            mpsc::unbounded_channel::<Result<PreparedFileShare, String>>();

        let settings: AppSettings = if let Some(storage) = cc.storage {
            eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default()
        } else {
            AppSettings::default()
        };

        // Build profiles list: use saved connections if present, else migrate legacy fields
        let mut profiles: Vec<ConnectionProfile> = settings.connections.clone();
        if let Some(legacy_profile) = migrate_legacy_profile(&settings) {
            profiles.push(legacy_profile);
        }

        // Apply even when no custom face is selected: `fonts::apply` also
        // installs the Unicode fallback used by federated display names.
        crate::ui::fonts::apply(&cc.egui_ctx, &settings.font_path);
        let available_fonts = crate::ui::fonts::scan_system_fonts();

        let adaptive_theme = settings.adaptive_theme;
        let cleared_buffer_names = restored_cleared_buffer_names(&settings);
        let wallpaper_rx = if adaptive_theme {
            Some(crate::ui::wallpaper::start_wallpaper_thread(cc.egui_ctx.clone()))
        } else {
            None
        };

        let mut app = Self {
            connections: Vec::new(),
            profiles,
            shared_event_tx,
            event_rx,
            show_connection_log: false,
            connection_log_unread: false,
            selected_conn_log: None,
            editing_profile: ConnectionProfile::default(),
            editing_password: String::new(),
            editing_ssh_password: String::new(),
            editing_profile_idx: None,
            show_connections: false,
            conn_show_add: false,
            conn_connect_idx: None,
            session_passwords: std::collections::HashMap::new(),
            session_ssh_passwords: std::collections::HashMap::new(),
            buffers: Vec::new(),
            buffer_idx: HashMap::new(),
            selected_buffer_id: None,
            last_chat_buffer_name: settings.last_chat_buffer_name,
            input_text: String::new(),
            show_settings: false,
            show_filtered_lines: settings.show_filtered_lines,
            colored_nicks: settings.colored_nicks,
            theme: settings.theme,
            font_size: settings.font_size,
            use_monospace: settings.use_monospace,
            show_timestamps: settings.show_timestamps,
            show_buffers: settings.show_buffers,
            show_nicklist: settings.show_nicklist,
            show_toolbar: settings.show_toolbar,
            nicklist_width: settings.nicklist_width,
            buffers_width: settings.buffers_width,
            auto_reconnect: settings.auto_reconnect,
            show_titlebar: settings.show_titlebar,
            show_server_headers: settings.show_server_headers,
            show_inline_images: settings.show_inline_images,
            show_link_previews: settings.show_link_previews,
            emoji_rendering: settings.emoji_rendering,
            opacity: settings.opacity,
            show_hidden_buffers: settings.show_hidden_buffers,
            image_cache: HashMap::new(),
            pending_matrix_media: HashSet::new(),
            avatar_texture_cache: HashMap::new(),
            avatar_image_keys: HashSet::new(),
            image_expanded: HashSet::new(),
            image_full_size: HashSet::new(),
            image_tx,
            image_rx,
            preview_cache: HashMap::new(),
            preview_expanded: HashSet::new(),
            preview_tx,
            preview_rx,
            completion: None,
            mention_completion: None,
            selected_mentions: Vec::new(),
            command_completion: None,
            command_completion_pending: None,
            command_completion_request_seq: 0,
            command_history: VecDeque::new(),
            history_index: None,
            focus_input: false,
            reply_target: None,
            profile_card: None,
            open_thread_buffer_id: None,
            open_thread_snapshot: None,
            thread_input_text: String::new(),
            focus_thread_input: false,
            thread_panel_width: THREAD_PANEL_DEFAULT_WIDTH,
            show_search: false,
            search_text: String::new(),
            pending_buffer_switch: None,
            buffer_order: settings.buffer_order,
            dragging_buffer_id: None,
            drag_drop_before_id: None,
            // Legacy persisted relay IDs are deliberately discarded: they may now identify
            // unrelated buffers in a restarted WeeChat process.
            cleared_buffer_ids: HashSet::new(),
            cleared_buffer_names,
            read_markers: settings.read_markers,
            font_name: settings.font_name,
            font_path: settings.font_path.clone(),
            applied_font_path: settings.font_path,
            available_fonts,
            selected_view_since: None,
            muted_buffer_names: settings.muted_buffer_names,
            last_notif_at: HashMap::new(),
            request_attention: false,
            notify_initialized: false,
            loading_more_buffer_id: None,
            history_request_counts: HashMap::new(),
            history_exhausted_buffer_ids: HashSet::new(),
            history_scroll_anchors: HashMap::new(),
            history_top_armed_buffer_ids: HashSet::new(),
            force_scroll_to_bottom_buffer_id: None,
            font_search: String::new(),
            prefix_align_max: settings.prefix_align_max,
            prefix_suffix: settings.prefix_suffix,
            prefix_col_widths: HashMap::new(),
            ctx_menu_hovered_url: None,
            np_tx,
            np_rx,
            sysinfo_tx,
            sysinfo_rx,
            file_share_tx,
            file_share_rx,
            file_share_uploading: false,
            file_share_status: None,
            file_share_target_buffer_id: None,
            file_share_expected_filename: None,
            file_share_started_at: None,
            file_share_error: None,
            file_share_duration: settings.file_share_duration,
            pending_redaction: None,
            pending_redaction_error: None,
            keybinds: settings.keybinds,
            editing_keybind: None,
            collapsed_servers: settings.collapsed_servers,
            adaptive_theme,
            adaptive_theme_result: None,
            wallpaper_rx,
        };

        // Trigger autoconnect. Legacy profiles may already have a keyring secret even if
        // their old settings file did not keep the save-password checkbox enabled.
        let auto_profiles: Vec<ConnectionProfile> = app.profiles.iter()
            .filter(|p| p.auto_connect)
            .cloned()
            .collect();
        for profile in auto_profiles {
            let password = load_profile_password(&profile).unwrap_or_default();
            app.do_connect(&profile, password, &cc.egui_ctx);
        }

        app
    }

    pub(crate) fn hash_nick(name: &str) -> u8 {
        let mut h: u32 = 0;
        for b in name.as_bytes() {
            h = h.wrapping_mul(31).wrapping_add(*b as u32);
        }
        ((h % 15) + 1) as u8
    }

    pub(crate) fn draw_sidebar_icon(painter: &Painter, rect: Rect, color: Color32, is_right: bool) {
        let stroke = Stroke::new(1.5, color);
        let rounding = Rounding::same(2.0);
        painter.rect_stroke(rect.shrink(4.0), rounding, stroke);

        let split_x = if is_right { rect.right() - 8.0 } else { rect.left() + 8.0 };
        painter.line_segment(
            [egui::pos2(split_x, rect.top() + 4.0), egui::pos2(split_x, rect.bottom() - 4.0)],
            stroke
        );
    }

    pub(crate) fn is_twemoji_url(url: &str) -> bool {
        url.contains("cdnjs.cloudflare.com/ajax/libs/twemoji")
    }

    pub(crate) fn is_image_url(url: &str) -> bool {
        let filename = url
            .split_once('#')
            .map(|(_, fragment)| fragment)
            .unwrap_or_else(|| url.split('?').next().unwrap_or(url))
            .to_lowercase();
        matches!(
            std::path::Path::new(&filename).extension().and_then(|e| e.to_str()),
            Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp")
        )
    }

    pub(crate) fn is_any_connected(&self) -> bool {
        self.connections.iter().any(|c| c.client.is_connected())
    }

    /// Rebuild the `buffer_idx` map from `self.buffers`. Call after any
    /// push/retain/extend/sort/clear of `self.buffers`.
    pub(crate) fn rebuild_buffer_idx(&mut self) {
        self.buffer_idx.clear();
        self.buffer_idx.reserve(self.buffers.len());
        for (i, b) in self.buffers.iter().enumerate() {
            self.buffer_idx.insert(b.id.clone(), i);
        }
    }

    /// O(1) lookup of a buffer's index by id.
    pub(crate) fn buffer_idx_of(&self, id: &str) -> Option<usize> {
        self.buffer_idx.get(id).copied()
    }

    pub(crate) fn buffer_by_id(&self, id: &str) -> Option<&Buffer> {
        self.buffer_idx_of(id).map(|i| &self.buffers[i])
    }

    pub(crate) fn buffer_by_id_mut(&mut self, id: &str) -> Option<&mut Buffer> {
        self.buffer_idx_of(id).map(move |i| &mut self.buffers[i])
    }

    /// Returns (client_ref, raw_buffer_id) for the connection that owns the given full prefixed buffer_id.
    pub(crate) fn client_for_buffer<'a>(&'a self, buffer_id: &str) -> Option<(&'a dyn BackendClient, String)> {
        for conn in &self.connections {
            let p = format!("{}/", conn.prefix);
            if let Some(raw) = buffer_id.strip_prefix(&p) {
                return Some((&*conn.client, raw.to_string()));
            }
        }
        None
    }

    fn is_matrix_buffer(&self, buffer_id: &str) -> bool {
        self
            .buffer_by_id(buffer_id)
            .is_some_and(|buffer| buffer.plugin == "matrix")
    }

    fn supports_matrix_upload(&self, buffer_id: &str) -> bool {
        buffer_supports_matrix_upload(self.buffer_by_id(buffer_id))
    }

    fn send_matrix_redaction(
        &self,
        buffer_id: &str,
        event_id: &str,
    ) -> Result<(), String> {
        if !self.is_matrix_buffer(buffer_id) {
            return Err("Deletion is available only for Matrix messages".to_owned());
        }
        let command = matrix_redact_command(event_id)?;
        let Some((client, raw_id)) = self.client_for_buffer(buffer_id) else {
            return Err("Matrix buffer has no authenticated relay connection".to_owned());
        };
        client.send_message(&raw_id, &command);
        Ok(())
    }

    fn begin_file_share_feedback(&mut self, buffer_id: String, status: &str) {
        self.file_share_uploading = true;
        self.file_share_target_buffer_id = Some(buffer_id);
        self.file_share_expected_filename = None;
        self.file_share_started_at = Some(std::time::Instant::now());
        self.file_share_status = Some(status.to_owned());
        self.file_share_error = None;
    }

    fn finish_file_share_feedback(&mut self, status: Option<String>) {
        self.file_share_uploading = false;
        self.file_share_started_at = None;
        self.file_share_expected_filename = None;
        self.file_share_status = status;
    }

    pub(crate) fn acknowledge_matrix_attachment(
        &mut self,
        buffer_id: &str,
        media: Option<&MatrixMedia>,
        is_self_msg: bool,
    ) {
        if matrix_attachment_echo_matches(
            self.file_share_target_buffer_id.as_deref(),
            self.file_share_expected_filename.as_deref(),
            buffer_id,
            media,
            is_self_msg,
        ) {
            let filename = self
                .file_share_expected_filename
                .clone()
                .unwrap_or_else(|| "attachment".to_owned());
            self.file_share_error = None;
            self.finish_file_share_feedback(Some(format!("✓ Sent {filename}")));
        }
    }

    fn send_matrix_attachment(
        &self,
        buffer_id: &str,
        filename: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        if !self.is_matrix_buffer(&buffer_id) {
            return Err("Attachment target is not a Matrix buffer".to_owned());
        }
        if !self.supports_matrix_upload(&buffer_id) {
            return Err(
                "Cannot send this attachment: the Matrix backend does not support native uploads. Update or restart the Matrix plugin."
                    .to_owned(),
            );
        }
        let Some((client, raw_id)) = self.client_for_buffer(&buffer_id) else {
            return Err("Matrix buffer has no authenticated relay connection".to_owned());
        };
        for command in matrix_attachment_upload(
            next_matrix_upload_id(),
            filename,
            mime,
            bytes,
        )? {
            client.send_message(&raw_id, &command);
        }
        Ok(())
    }

    fn start_matrix_clipboard_upload(
        &mut self,
        buffer_id: String,
        ctx: &egui::Context,
    ) -> bool {
        if self.file_share_uploading {
            self.file_share_error = Some(
                "Cannot paste while another attachment is being prepared or uploaded".to_owned(),
            );
            return false;
        }
        if !self.is_matrix_buffer(&buffer_id) {
            self.file_share_error = Some(
                "Cannot paste an image here: native clipboard image upload is available in Matrix chats"
                    .to_owned(),
            );
            return false;
        }
        if !self.supports_matrix_upload(&buffer_id) {
            self.file_share_error = Some(
                "Cannot paste this image: the Matrix backend does not support native uploads. Update or restart the Matrix plugin."
                    .to_owned(),
            );
            return false;
        }
        self.begin_file_share_feedback(buffer_id.clone(), "Reading clipboard image…");
        let tx = self.file_share_tx.clone();
        let repaint = ctx.clone();
        tokio::spawn(async move {
            let result = match tokio::task::spawn_blocking(crate::ui::fileshare::clipboard_png)
                .await
            {
                Ok(Ok(Some(bytes))) if bytes.len() <= MATRIX_UPLOAD_MAX_BYTES => {
                    Ok(PreparedFileShare::MatrixAttachment {
                        buffer_id,
                        filename: "clipboard.png".to_owned(),
                        mime: "image/png".to_owned(),
                        bytes,
                    })
                }
                Ok(Ok(Some(_))) => Err(format!(
                    "Matrix clipboard images may not exceed {} MiB",
                    MATRIX_UPLOAD_MAX_BYTES / 1024 / 1024,
                )),
                Ok(Ok(None)) => Err(
                    "Cannot paste: clipboard text was not accepted by the input field".to_owned(),
                ),
                Ok(Err(error)) => Err(error),
                Err(error) => Err(format!("Clipboard worker failed: {error}")),
            };
            let _ = tx.send(result);
            repaint.request_repaint();
        });
        true
    }

    fn start_file_picker(&mut self, buffer_id: String) {
        if self.file_share_uploading {
            self.file_share_error = Some(
                "Another attachment is still being prepared or uploaded".to_owned(),
            );
            return;
        }
        self.begin_file_share_feedback(buffer_id.clone(), "Choose an attachment…");
        let is_matrix = self.is_matrix_buffer(&buffer_id);
        let duration = self.file_share_duration.clone();
        let tx = self.file_share_tx.clone();
        tokio::spawn(async move {
            let Some(handle) = rfd::AsyncFileDialog::new().pick_file().await else {
                let _ = tx.send(Err(String::new()));
                return;
            };
            let result = prepare_file_share(
                buffer_id,
                handle.path().to_path_buf(),
                is_matrix,
                duration,
            )
            .await;
            let _ = tx.send(result);
        });
    }

    fn start_file_path(&mut self, buffer_id: String, path: PathBuf) {
        if self.file_share_uploading {
            self.file_share_error = Some(
                "Another attachment is still being prepared or uploaded".to_owned(),
            );
            return;
        }
        self.begin_file_share_feedback(buffer_id.clone(), "Preparing attachment…");
        let is_matrix = self.is_matrix_buffer(&buffer_id);
        let duration = self.file_share_duration.clone();
        let tx = self.file_share_tx.clone();
        tokio::spawn(async move {
            let result =
                prepare_file_share(buffer_id, path, is_matrix, duration).await;
            let _ = tx.send(result);
        });
    }

    fn backend_type_for_buffer(&self, buffer_id: &str) -> Option<BackendType> {
        self.connections.iter().find_map(|conn| {
            let prefix = format!("{}/", conn.prefix);
            buffer_id
                .starts_with(&prefix)
                .then(|| conn.backend_type.clone())
        })
    }

    fn request_older_history(&mut self, buffer_id: &str, view_buffer_id: &str) {
        if self.loading_more_buffer_id.is_some()
            || self.history_exhausted_buffer_ids.contains(buffer_id)
        {
            return;
        }

        let Some(buffer) = self.buffer_by_id(buffer_id) else {
            return;
        };
        let current_len = buffer.messages.len();
        let (view_anchor, view_len) = if view_buffer_id == buffer_id {
            (buffer.messages.front().map(|line| line.id.clone()), current_len)
        } else {
            let (messages, _) = composed_upgrade_history(&self.buffers, view_buffer_id);
            (messages.front().map(|line| line.id.clone()), messages.len())
        };
        let oldest_timestamp = buffer.messages.front().map(|line| {
            line.timestamp
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string()
        });
        let is_matrix = buffer.plugin == "matrix";
        let previous_request = self
            .history_request_counts
            .get(buffer_id)
            .copied()
            .unwrap_or(current_len);
        let backend_type = self.backend_type_for_buffer(buffer_id);

        let request_count = if is_matrix || backend_type == Some(BackendType::Soju) {
            None
        } else {
            next_history_request_count(current_len, previous_request)
        };
        if !is_matrix && backend_type != Some(BackendType::Soju) && request_count.is_none() {
            self.history_exhausted_buffer_ids.insert(buffer_id.to_owned());
            return;
        }

        self.loading_more_buffer_id = Some(buffer_id.to_owned());
        self.history_top_armed_buffer_ids.remove(buffer_id);
        if let Some(anchor) = view_anchor {
            self.history_scroll_anchors
                .insert(view_buffer_id.to_owned(), (anchor, view_len));
        }
        if let Some(count) = request_count {
            self.history_request_counts
                .insert(buffer_id.to_owned(), count);
        } else if is_matrix {
            self.history_request_counts
                .entry(buffer_id.to_owned())
                .or_insert(current_len.max(INITIAL_LINES));
        }

        if let Some((client, raw_id)) = self.client_for_buffer(buffer_id) {
            if is_matrix {
                client.send_message(&raw_id, "/matrix history");
            } else if backend_type == Some(BackendType::Soju) {
                if let Some(timestamp) = oldest_timestamp {
                    client.fetch_lines_before(&raw_id, &timestamp);
                }
            } else if let Some(count) = request_count {
                client.fetch_lines(&raw_id, count);
            }
        } else {
            self.loading_more_buffer_id = None;
        }
    }

    fn ensure_matrix_media_loading(
        &mut self,
        buffer_id: &str,
        media: &MatrixMedia,
    ) {
        let cache_key = media.mxc_uri.clone();
        self.image_expanded.insert(cache_key.clone());
        if !begin_matrix_media_load(
            &mut self.image_cache,
            &mut self.pending_matrix_media,
            &cache_key,
        ) {
            return;
        }

        let path = matrix_media_cache_path(&media.mxc_uri);
        let parent_ready = path
            .parent()
            .ok_or_else(|| "Matrix media cache has no parent".to_owned())
            .and_then(|parent| {
                std::fs::create_dir_all(parent)
                    .map_err(|error| error.to_string())?;
                #[cfg(unix)]
                std::fs::set_permissions(
                    parent,
                    std::fs::Permissions::from_mode(0o700),
                )
                .map_err(|error| error.to_string())?;
                Ok(())
            });
        if let Err(error) = parent_ready {
            let _ = self.image_tx.send((cache_key, Err(error)));
            return;
        }

        if !path.is_file() {
            if let Some((client, raw_buffer_id)) =
                self.client_for_buffer(buffer_id)
            {
                let command = format!(
                    "/matrix media download {} {}",
                    media.mxc_uri,
                    quote_weechat_argument(&path.display().to_string())
                );
                client.send_message(&raw_buffer_id, &command);
            }
        }

        let tx = self.image_tx.clone();
        tokio::spawn(async move {
            let result = wait_for_matrix_media(&path).await;
            let _ = tx.send((cache_key, result));
        });
    }

    fn ensure_image_loading(&mut self, url: &str) {
        self.image_expanded.insert(url.to_owned());
        if self.image_cache.contains_key(url) || !is_safe_public_url(url) {
            return;
        }
        self.image_cache.insert(url.to_owned(), ImageState::Loading);
        let tx = self.image_tx.clone();
        let url = url.to_owned();
        tokio::spawn(async move {
            let result = fetch_bounded_image(&url).await;
            let _ = tx.send((url, result));
        });
    }

    fn ensure_link_preview_loading(&mut self, url: &str) {
        if self.preview_cache.contains_key(url) || !is_safe_public_url(url) {
            return;
        }
        self.preview_cache
            .insert(url.to_owned(), PreviewState::Loading);
        let tx = self.preview_tx.clone();
        let url = url.to_owned();
        tokio::spawn(async move {
            let result = fetch_link_preview(url.clone()).await;
            let _ = tx.send((url, result));
        });
    }

    fn text_with_emoji_width(
        &self,
        ui: &egui::Ui,
        text: &str,
        font_id: &FontId,
        force_emoji: bool,
    ) -> f32 {
        if !self.emoji_rendering && !force_emoji {
            return ui.fonts(|fonts| {
                fonts
                    .layout_no_wrap(text.to_owned(), font_id.clone(), Color32::WHITE)
                    .size()
                    .x
            });
        }

        ui.fonts(|fonts| {
            crate::ui::emoji::split_emoji(text)
                .into_iter()
                .map(|span| match span {
                    crate::ui::emoji::TextSpan::Text(text) => fonts
                        .layout_no_wrap(text, font_id.clone(), Color32::WHITE)
                        .size()
                        .x,
                    crate::ui::emoji::TextSpan::Emoji(_) => font_id.size + 2.0,
                })
                .sum()
        })
    }

    fn render_text_with_emoji(
        &mut self,
        ui: &mut egui::Ui,
        text: &str,
        format: &egui::TextFormat,
        wrap: bool,
        force_emoji: bool,
    ) {
        if !self.emoji_rendering && !force_emoji {
            let mut job = LayoutJob::default();
            job.append(text, 0.0, format.clone());
            ui.add(Label::new(job).wrap(wrap));
            return;
        }

        let emoji_size = format.font_id.size + 2.0;
        for span in crate::ui::emoji::split_emoji(text) {
            match span {
                crate::ui::emoji::TextSpan::Text(text) => {
                    let mut job = LayoutJob::default();
                    job.append(&text, 0.0, format.clone());
                    ui.add(Label::new(job).wrap(wrap));
                }
                crate::ui::emoji::TextSpan::Emoji(emoji) => {
                    let url = crate::ui::emoji::emoji_to_twemoji_url(&emoji);
                    if !self.image_cache.contains_key(&url) {
                        self.image_cache.insert(url.clone(), ImageState::Loading);
                        let tx = self.image_tx.clone();
                        let url_owned = url.clone();
                        tokio::spawn(async move {
                            let result = async {
                                let bytes = reqwest::get(&url_owned)
                                    .await
                                    .map_err(|error| error.to_string())?
                                    .bytes()
                                    .await
                                    .map_err(|error| error.to_string())?;
                                Ok(bytes.to_vec())
                            }
                            .await;
                            let _ = tx.send((url_owned, result));
                        });
                    }
                    if let Some(ImageState::Loaded(texture)) = self.image_cache.get(&url) {
                        ui.add(egui::Image::new((
                            texture.id(),
                            egui::Vec2::splat(emoji_size),
                        )));
                    } else {
                        // Keep the prefix column stable while the Twemoji asset
                        // is loading. A font-fallback label has a different
                        // advance on many systems and used to make the message
                        // separator jump when the texture arrived.
                        let (rect, _) = ui.allocate_exact_size(
                            egui::Vec2::splat(emoji_size),
                            egui::Sense::hover(),
                        );
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            emoji,
                            format.font_id.clone(),
                            format.color,
                        );
                    }
                }
            }
        }
    }

    fn render_profile_identity(
        &mut self,
        ui: &mut egui::Ui,
        text: &str,
        mut font_id: FontId,
        color: Color32,
    ) {
        // Profile identities come from federated user input and routinely use
        // IPA, combining marks, and scripts outside egui's bundled fonts.
        // Render their text spans with the broad system face explicitly;
        // actual emoji are still replaced by Twemoji below.
        font_id.family = egui::FontFamily::Name("unicode_fallback".into());
        let width = self.text_with_emoji_width(ui, text, &font_id, true);
        let format = egui::TextFormat::simple(font_id.clone(), color);
        ui.allocate_ui_with_layout(
            egui::vec2(width, font_id.size + 2.0),
            prefix_span_layout(),
            |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                self.render_text_with_emoji(ui, text, &format, false, true);
            },
        );
    }

    fn render_matrix_avatar(
        &mut self,
        ui: &mut egui::Ui,
        buffer_id: &str,
        profiles: &[MatrixMemberProfile],
        nick: &str,
        size: f32,
        accent_color: Color32,
        text_color: Color32,
    ) -> egui::Response {
        let (rect, response) =
            ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
        let profile = matrix_profile_for_nick(profiles, nick);
        if let Some(avatar_mxc) = profile
            .as_ref()
            .and_then(|profile| profile.avatar_mxc.as_ref())
        {
            self.avatar_image_keys.insert(avatar_mxc.clone());
            if ui.is_rect_visible(rect) && !self.image_cache.contains_key(avatar_mxc) {
                self.ensure_matrix_media_loading(
                    buffer_id,
                    &MatrixMedia {
                        mxc_uri: avatar_mxc.clone(),
                        name: "member-avatar".to_owned(),
                        kind: "image".to_owned(),
                    },
                );
            }
            let texture = self.avatar_texture_cache.get(avatar_mxc).or_else(|| {
                match self.image_cache.get(avatar_mxc) {
                    Some(ImageState::Loaded(texture)) => Some(texture),
                    _ => None,
                }
            });
            if let Some(texture) = texture {
                ui.put(
                    rect,
                    egui::Image::new((texture.id(), egui::Vec2::splat(size)))
                        .rounding(size / 2.0),
                );
                return response;
            }
        }

        ui.painter()
            .circle_filled(rect.center(), size / 2.0, accent_color.gamma_multiply(0.45));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            profile
                .as_ref()
                .map(|profile| profile.display_name.as_str())
                .unwrap_or(nick)
                .chars()
                .find(|character| character.is_alphanumeric())
                .map(|character| character.to_uppercase().to_string())
                .unwrap_or_else(|| "?".to_owned()),
            FontId::new(size * 0.48, FontFamily::Proportional),
            text_color,
        );
        response
    }

    fn open_profile_on_author_click(
        &mut self,
        ui: &mut egui::Ui,
        id: egui::Id,
        rect: Rect,
        card: Option<UserProfileCard>,
    ) {
        let Some(card) = card else { return };
        let response = ui
            .interact(rect.expand(2.0), id, egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("View profile");
        if response_primary_clicked(ui, &response) {
            self.profile_card = Some(card);
        }
    }

    fn render_message_content(
        &mut self,
        ui: &mut egui::Ui,
        message: &str,
        matrix_media: Option<&MatrixMedia>,
        matrix_buffer_id: Option<&str>,
        font_id: &FontId,
        render_theme: &AppTheme,
    ) -> MessagePreviewTargets {
        let matrix_image = matrix_media.filter(|media| media.kind == "image");
        if self.show_inline_images {
            if let (Some(buffer_id), Some(media)) = (matrix_buffer_id, matrix_image) {
                if !self.image_cache.contains_key(&media.mxc_uri) {
                    self.ensure_matrix_media_loading(buffer_id, media);
                }
            }
        }

        let rendered_message = if self.show_inline_images {
            matrix_image
                .map(|media| format!("📎 {}", media.name))
                .unwrap_or_else(|| message.to_owned())
        } else {
            message.to_owned()
        };
        let sections = ANSIParser::parse(&rendered_message);
        let urls: Vec<String> = sections
            .iter()
            .filter_map(|section| section.url.clone())
            .filter(|url| !Self::is_twemoji_url(url) && is_safe_public_url(url))
            .collect();
        let image_urls = if self.show_inline_images {
            urls.iter()
                .filter(|url| Self::is_image_url(url))
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for url in &image_urls {
            if !self.image_cache.contains_key(url) {
                self.ensure_image_loading(url);
            }
        }
        let preview_urls = if self.show_link_previews {
            urls.iter()
                .filter(|url| !Self::is_image_url(url))
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut hovered_url = None;

        if !rendered_message.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for section in &sections {
                    if let Some(url) = &section.url {
                        let link =
                            ui.link(egui::RichText::new(&section.text).font(font_id.clone()));
                        if link.hovered() {
                            hovered_url = Some(url.clone());
                        }
                        if response_primary_clicked(ui, &link) {
                            ui.ctx().output_mut(|output| {
                                open_url_once(output, url);
                            });
                        }
                        if self.show_inline_images && Self::is_image_url(url) {
                            let expanded = self.image_expanded.contains(url);
                            let button =
                                ui.small_button(if expanded { "🖼" } else { "🖼 preview" });
                            if response_primary_clicked(ui, &button) {
                                if expanded {
                                    self.image_expanded.remove(url);
                                    self.image_full_size.remove(url);
                                } else {
                                    self.ensure_image_loading(url);
                                }
                            }
                        }
                        if self.show_link_previews && !Self::is_image_url(url) {
                            let expanded = self.preview_expanded.contains(url);
                            let button =
                                ui.small_button(if expanded { "🔗" } else { "🔗 preview" });
                            if response_primary_clicked(ui, &button) {
                                if expanded {
                                    self.preview_expanded.remove(url);
                                } else {
                                    self.preview_expanded.insert(url.clone());
                                    self.ensure_link_preview_loading(url);
                                }
                            }
                        }
                    } else {
                        let format = section.style.to_format(font_id.clone(), render_theme);
                        self.render_text_with_emoji(ui, &section.text, &format, true, false);
                    }
                }
            });
        }

        MessagePreviewTargets {
            image_urls,
            preview_urls,
            matrix_image_key: matrix_image.map(|media| media.mxc_uri.clone()),
            hovered_url,
        }
    }

    fn render_image_preview(&mut self, ui: &mut egui::Ui, cache_key: &str, text_muted: Color32) {
        ui.add_space(4.0);
        match self.image_cache.get(cache_key) {
            Some(ImageState::Loaded(texture)) => {
                let original = texture.size_vec2();
                let expanded = self.image_full_size.contains(cache_key);
                let size = inline_image_display_size(
                    texture.size_vec2(),
                    ui.available_width(),
                    ui.clip_rect().height(),
                    expanded,
                );
                let response = Frame::none()
                    .fill(Color32::BLACK.linear_multiply(0.18))
                    .rounding(Rounding::same(7.0))
                    .inner_margin(Margin::same(4.0))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Image::new((texture.id(), size))
                                .rounding(4.0)
                                .sense(egui::Sense::click()),
                        )
                    })
                    .inner;
                if response_primary_clicked(ui, &response) {
                    if expanded {
                        self.image_full_size.remove(cache_key);
                    } else {
                        self.image_full_size.insert(cache_key.to_owned());
                    }
                }
                ui.label(
                    egui::RichText::new(format!(
                        "{} × {}  ·  click to {}",
                        original.x as usize,
                        original.y as usize,
                        if expanded { "shrink" } else { "enlarge" },
                    ))
                    .color(text_muted)
                    .small(),
                );
            }
            Some(ImageState::Loading) | None => {
                ui.label(
                    egui::RichText::new("Loading image…")
                        .color(text_muted)
                        .italics()
                        .small(),
                );
            }
            Some(ImageState::Failed) => {
                ui.label(
                    egui::RichText::new("Failed to load image")
                        .color(Color32::from_rgb(220, 80, 80))
                        .small(),
                );
            }
        }
        ui.add_space(4.0);
    }

    fn render_message_previews(
        &mut self,
        ui: &mut egui::Ui,
        previews: &MessagePreviewTargets,
        text_secondary: Color32,
        text_muted: Color32,
        card_bg: Color32,
        border_color: Color32,
        accent_color: Color32,
    ) {
        if self.show_inline_images {
            for url in &previews.image_urls {
                if self.image_expanded.contains(url) {
                    self.render_image_preview(ui, url, text_muted);
                }
            }
            if let Some(cache_key) = &previews.matrix_image_key {
                self.render_image_preview(ui, cache_key, text_muted);
            }
        }
        if self.show_link_previews {
            for url in &previews.preview_urls {
                if !self.preview_expanded.contains(url) {
                    continue;
                }
                ui.add_space(4.0);
                match self.preview_cache.get(url) {
                    Some(PreviewState::Loading) | None => {
                        ui.label(
                            egui::RichText::new("Loading preview…")
                                .color(text_muted)
                                .italics()
                                .small(),
                        );
                    }
                    Some(PreviewState::Failed) => {
                        ui.label(
                            egui::RichText::new("No preview available")
                                .color(text_muted)
                                .small(),
                        );
                    }
                    Some(PreviewState::Loaded(preview)) => {
                        let card = Frame::none()
                            .fill(card_bg)
                            .rounding(Rounding::same(6.0))
                            .stroke(Stroke::new(1.0, border_color))
                            .inner_margin(Margin {
                                left: 14.0,
                                right: 12.0,
                                top: 8.0,
                                bottom: 8.0,
                            })
                            .show(ui, |ui| {
                                ui.set_max_width(ui.available_width().min(520.0));
                                if let Some(site) = &preview.site_name {
                                    ui.label(egui::RichText::new(site).small().color(text_muted));
                                }
                                if let Some(title) = &preview.title {
                                    ui.label(egui::RichText::new(title).strong());
                                }
                                if let Some(description) = &preview.description {
                                    let mut chars = description.chars();
                                    let mut text: String = chars.by_ref().take(240).collect();
                                    if chars.next().is_some() {
                                        text.push('…');
                                    }
                                    ui.label(
                                        egui::RichText::new(text).small().color(text_secondary),
                                    );
                                }
                                if let Some(image_url) = &preview.image_url {
                                    if let Some(ImageState::Loaded(texture)) =
                                        self.image_cache.get(image_url)
                                    {
                                        let size = inline_image_preview_size(
                                            texture.size_vec2(),
                                            ui.available_width(),
                                            ui.clip_rect().height(),
                                        );
                                        ui.add_space(4.0);
                                        ui.add(
                                            egui::Image::new((texture.id(), size)).rounding(4.0),
                                        );
                                    }
                                }
                            });
                        let bar = egui::Rect::from_min_max(
                            card.response.rect.min,
                            egui::pos2(card.response.rect.min.x + 3.0, card.response.rect.max.y),
                        );
                        ui.painter()
                            .rect_filled(bar, Rounding::same(3.0), accent_color);
                    }
                }
                ui.add_space(4.0);
            }
        }
    }

    fn remember_buffer_read_marker(&mut self, id: &str) {
        let marker = self.buffer_by_id(id).and_then(|buffer| {
            buffer
                .messages
                .back()
                .map(|line| (buffer.full_name.clone(), SavedReadMarker::from_line(line)))
        });
        if let Some((full_name, marker)) = marker {
            self.advance_read_marker(full_name, marker);
        }
    }

    pub(crate) fn advance_read_marker(&mut self, full_name: String, marker: SavedReadMarker) {
        self.read_markers
            .entry(full_name)
            .and_modify(|saved| *saved = saved.advanced_with(marker.clone()))
            .or_insert(marker);
    }

    pub(crate) fn clear_buffer_activity_persistently(&mut self, id: &str) {
        self.cleared_buffer_ids.insert(id.to_owned());
        if let Some(full_name) = self.buffer_by_id(id).map(|buffer| buffer.full_name.clone()) {
            self.cleared_buffer_names.insert(full_name);
        }
    }

    pub(crate) fn select_buffer(&mut self, id: String) {
        if self
            .buffer_by_id(&id)
            .is_some_and(|buffer| buffer.is_matrix_thread())
        {
            return;
        }
        if let Some(prev_id) = self.selected_buffer_id.clone() {
            if prev_id != id {
                self.reply_target = None;
                self.mention_completion = None;
                self.selected_mentions.clear();
                self.profile_card = None;
                self.remember_buffer_read_marker(&prev_id);
                if let Some((client, raw_id)) = self.client_for_buffer(&prev_id) {
                    client.mark_read(&raw_id);
                }
            }
        }

        self.selected_buffer_id = Some(id.clone());
        if let Some(buffer) = self.buffer_by_id(&id).filter(|buffer| {
            is_restorable_chat_buffer(buffer)
        }) {
            self.last_chat_buffer_name = Some(buffer.full_name.clone());
        }
        self.focus_input = true;
        self.selected_view_since = Some(std::time::Instant::now());
        self.clear_buffer_activity_persistently(&id);
        if let Some(buffer) = self.buffer_by_id_mut(&id) {
            buffer.activity = BufferActivity::None;
            buffer.unread_count = 0;
            buffer.visit_start_marker_id = buffer.last_read_id.clone();
            let fetch_nicks = buffer.has_nicklist;
            if let Some((client, raw_id)) = self.client_for_buffer(&id) {
                client.refresh_buffer(&raw_id);
                client.fetch_lines(&raw_id, INITIAL_LINES);
                if fetch_nicks {
                    client.fetch_nicks(&raw_id);
                }
                client.mark_read(&raw_id);
            }
        }
        self.remember_buffer_read_marker(&id);
    }

    pub(crate) fn open_thread(&mut self, buffer_id: String) {
        let Some(thread) = self
            .buffer_by_id(&buffer_id)
            .filter(|buffer| buffer.is_matrix_thread())
            .cloned()
        else {
            return;
        };
        let needs_lines = thread.messages.is_empty();
        if self.open_thread_buffer_id.as_deref() != Some(buffer_id.as_str()) {
            self.thread_input_text.clear();
        }
        self.open_thread_buffer_id = Some(buffer_id.clone());
        self.open_thread_snapshot = Some(thread);
        self.focus_thread_input = false;
        if let Some(buffer) = self.buffer_by_id_mut(&buffer_id) {
            buffer.activity = BufferActivity::None;
            buffer.unread_count = 0;
        }
        self.clear_buffer_activity_persistently(&buffer_id);
        self.remember_buffer_read_marker(&buffer_id);
        if let Some((client, raw_id)) = self.client_for_buffer(&buffer_id) {
            if needs_lines {
                client.fetch_lines(&raw_id, INITIAL_LINES);
            }
            client.mark_read(&raw_id);
        }
    }

    fn close_thread(&mut self) {
        self.open_thread_buffer_id = None;
        self.open_thread_snapshot = None;
        self.thread_input_text.clear();
        self.focus_thread_input = false;
    }

    pub(crate) fn log_conn_for(&mut self, conn_prefix: &str, msg: impl Into<String>) {
        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
        let text = format!("[{}]  {}", ts, msg.into());
        if let Some(conn) = self.connections.iter_mut().find(|c| c.prefix == conn_prefix) {
            conn.connection_log.push_back(text.clone());
            if conn.connection_log.len() > 500 { conn.connection_log.pop_front(); }
        }
        if !self.show_connection_log {
            self.connection_log_unread = true;
        }
    }

    /// Start a connection for the given profile with the given password.
    pub(crate) fn do_connect(&mut self, profile: &ConnectionProfile, password: String, ctx: &egui::Context) {
        let prefix = profile.prefix();
        // Remove any stale handle with same prefix
        self.connections.retain(|c| c.prefix != prefix);

        let (per_conn_tx, per_conn_rx) = mpsc::unbounded_channel::<BackendEvent>();
        let relay_port = profile.port.parse::<u16>().unwrap_or(9001);

        // Cache the relay password for this session so future connects skip the prompt.
        if !password.is_empty() {
            self.session_passwords.insert(prefix.clone(), password.clone());
        }

        // Start SSH tunnel if configured. Capture a log message for both success and failure.
        let ssh_password = self.session_ssh_passwords.get(&prefix).cloned()
            .or_else(|| if profile.ssh_save_password {
                crate::ui::secure_storage::load_by_key(&profile.ssh_keyring_key())
            } else { None });

        let mut ssh_tunnel_log: Option<String> = None;
        let (effective_host, effective_port, ssh_tunnel) = if profile.ssh_enabled && !profile.ssh_host.is_empty() {
            match crate::ui::ssh_tunnel::SshTunnel::spawn(
                &profile.ssh_host,
                profile.ssh_port,
                &profile.ssh_user,
                ssh_password.as_deref(),
                &profile.host,
                relay_port,
            ) {
                Ok(t) => {
                    let lp = t.local_port;
                    ssh_tunnel_log = Some(format!(
                        "SSH tunnel started: 127.0.0.1:{} → {}:{}",
                        lp, profile.host, relay_port
                    ));
                    ("127.0.0.1".to_string(), lp, Some(t))
                }
                Err(e) => {
                    ssh_tunnel_log = Some(format!("SSH tunnel failed to start: {} — connecting directly", e));
                    (profile.host.clone(), relay_port, None)
                }
            }
        } else {
            (profile.host.clone(), relay_port, None)
        };

        let (proto, path) = match profile.backend_type {
            BackendType::Soju    => (if profile.use_ssl { "ircs" } else { "irc" }, ""),
            BackendType::WeeChat => (if profile.use_ssl { "wss"  } else { "ws"  }, "/api"),
        };

        let tunnel_port = ssh_tunnel.as_ref().map(|t| t.local_port);
        let effective_auto_reconnect = profile.auto_reconnect && self.auto_reconnect;

        let mut client: Box<dyn BackendClient> = match profile.backend_type {
            BackendType::Soju => {
                let config = crate::relay::irc::IrcConfig {
                    label: profile.label.clone(),
                    host: effective_host.clone(),
                    port: effective_port,
                    nick: if profile.nick.is_empty() { "user".to_string() } else { profile.nick.clone() },
                    username: profile.username.clone(),
                    sasl_username: profile.sasl_username.clone(),
                    password: password.clone(),
                    use_ssl: profile.use_ssl,
                    accept_invalid_certs: profile.accept_invalid_certs,
                    channel: profile.channel.clone(),
                    tunnel_port,
                    auto_reconnect: effective_auto_reconnect,
                };
                Box::new(crate::relay::irc::IrcClient::new(config, per_conn_tx.clone(), ctx.clone()))
            }
            BackendType::WeeChat => {
                let config = WeeChatConfig {
                    host: effective_host.clone(),
                    port: effective_port,
                    password: password.clone(),
                    use_ssl: profile.use_ssl,
                    accept_invalid_certs: profile.accept_invalid_certs,
                    tunnel_port,
                    auto_reconnect: effective_auto_reconnect,
                };
                Box::new(WeeChatClient::new(config, per_conn_tx.clone(), ctx.clone()))
            }
        };
        client.connect();

        spawn_event_forwarder(prefix.clone(), per_conn_rx, self.shared_event_tx.clone());

        let mut conn = ConnectionHandle {
            prefix: prefix.clone(),
            label: profile.label.clone(),
            backend_type: profile.backend_type.clone(),
            client,
            status: "Connecting...".to_string(),
            is_connecting: true,
            connecting_pending: true,
            auth_error: None,
            auto_reconnect: effective_auto_reconnect,
            connection_log: VecDeque::new(),
            ssh_tunnel,
        };

        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
        if let Some(msg) = ssh_tunnel_log {
            conn.connection_log.push_back(format!("[{}]  {}", ts, msg));
        }
        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
        conn.connection_log.push_back(format!("[{}]  Connecting to {}://{}:{}{}", ts, proto, profile.host, relay_port, path));
        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
        conn.connection_log.push_back(format!("[{}]  SSL/TLS: {}", ts, if profile.use_ssl { "enabled" } else { "disabled" }));

        if self.selected_conn_log.is_none() {
            self.selected_conn_log = Some(prefix.clone());
        }

        self.connections.push(conn);
        if !self.show_connection_log {
            self.connection_log_unread = true;
        }
    }
}

impl eframe::App for WeeChatApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let settings = AppSettings {
            backend_type: BackendType::default(),
            host: String::new(),
            port: String::new(),
            use_ssl: false,
            show_filtered_lines: self.show_filtered_lines,
            colored_nicks: self.colored_nicks,
            theme: self.theme.clone(),
            font_size: self.font_size,
            use_monospace: self.use_monospace,
            show_timestamps: self.show_timestamps,
            show_buffers: self.show_buffers,
            show_nicklist: self.show_nicklist,
            show_toolbar: self.show_toolbar,
            nicklist_width: self.nicklist_width,
            buffers_width: self.buffers_width,
            auto_reconnect: self.auto_reconnect,
            show_titlebar: self.show_titlebar,
            show_server_headers: self.show_server_headers,
            show_inline_images: self.show_inline_images,
            show_link_previews: self.show_link_previews,
            emoji_rendering: self.emoji_rendering,
            opacity: self.opacity,
            show_hidden_buffers: self.show_hidden_buffers,
            buffer_order: self.buffer_order.clone(),
            // IDs are runtime-only. Stable names below are the reload-safe source of truth.
            cleared_buffer_ids: HashSet::new(),
            cleared_buffer_names: self.cleared_buffer_names.clone(),
            read_markers: self.read_markers.clone(),
            save_password: false,
            font_name: self.font_name.clone(),
            font_path: self.font_path.clone(),
            muted_buffer_names: self.muted_buffer_names.clone(),
            accept_invalid_certs: false,
            irc_nick: String::new(),
            connections: self.profiles.clone(),
            prefix_align_max: self.prefix_align_max,
            prefix_suffix: self.prefix_suffix.clone(),
            file_share_duration: self.file_share_duration.clone(),
            keybinds: self.keybinds.clone(),
            collapsed_servers: self.collapsed_servers.clone(),
            adaptive_theme: self.adaptive_theme,
            last_chat_buffer_name: self.last_chat_buffer_name.clone(),
        };
        eframe::set_value(storage, eframe::APP_KEY, &settings);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Capture paste intent before TextEdit consumes the key event. The
        // event carries the modifiers from key-down even when Ctrl/Cmd was
        // released again before this frame is rendered.
        let (paste_shortcut_this_frame, paste_image_this_frame) = ctx.input(|input| {
            (
                paste_shortcut_pressed(&input.events),
                should_probe_clipboard_image(&input.events),
            )
        });
        if paste_shortcut_this_frame {
            log::debug!(
                "clipboard paste attempt captured: native_image_probe={paste_image_this_frame}"
            );
        }

        if !self.notify_initialized {
            self.notify_initialized = true;
            crate::ui::notify::init();
        }

        while let Ok((prefix, event)) = self.event_rx.try_recv() {
            self.handle_event(&prefix, event);
        }

        // Drain the wallpaper-watcher thread; update override when a new theme arrives.
        if let Some(rx) = &self.wallpaper_rx {
            while let Ok(theme) = rx.try_recv() {
                self.adaptive_theme_result = Some(theme);
            }
        }

        if self.request_attention {
            self.request_attention = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Critical,
            ));
        }

        // Periodically prune prefix_col_widths so it can't grow indefinitely as
        // transient buffers come and go. Cheap when below cap.
        if self.prefix_col_widths.len() > PREFIX_COL_WIDTHS_MAX {
            self.prefix_col_widths.retain(|id, _| self.buffer_idx.contains_key(id));
            // If still over cap (huge connected session), drop arbitrary entries.
            cap_map(&mut self.prefix_col_widths, PREFIX_COL_WIDTHS_MAX);
        }

        // Same for the notification cooldown map — drop entries older than 5 minutes.
        if self.last_notif_at.len() > 200 {
            let now = std::time::Instant::now();
            self.last_notif_at.retain(|_, t| now.duration_since(*t) < std::time::Duration::from_secs(300));
        }

        while let Ok((url, result)) = self.image_rx.try_recv() {
            self.pending_matrix_media.remove(&url);
            match result {
                Ok(bytes) => {
                    match validate_inline_image_dimensions(&bytes)
                        .and_then(|_| image::load_from_memory(&bytes).map_err(|error| error.to_string()))
                    {
                        Ok(img) => {
                            let avatar_texture = self.avatar_image_keys.contains(&url).then(|| {
                                let rgba = avatar_thumbnail(&img, 64);
                                let size = [rgba.width() as usize, rgba.height() as usize];
                                let color_img = egui::ColorImage::from_rgba_unmultiplied(
                                    size,
                                    rgba.as_raw(),
                                );
                                ctx.load_texture(
                                    format!("{url}#avatar-64"),
                                    color_img,
                                    egui::TextureOptions::LINEAR,
                                )
                            });
                            let rgba = img.to_rgba8();
                            let size = [rgba.width() as usize, rgba.height() as usize];
                            let color_img = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                            let handle = ctx.load_texture(&url, color_img, egui::TextureOptions::default());
                            if let Some(avatar_texture) = avatar_texture {
                                self.avatar_texture_cache.insert(url.clone(), avatar_texture);
                            }
                            self.image_cache.insert(url, ImageState::Loaded(handle));
                        }
                        Err(_) => {
                            self.image_full_size.remove(&url);
                            self.image_cache.insert(url, ImageState::Failed);
                        }
                    }
                }
                Err(_) => {
                    self.image_full_size.remove(&url);
                    self.image_cache.insert(url, ImageState::Failed);
                }
            }
            cap_map(&mut self.image_cache, IMAGE_CACHE_MAX);
            cap_map(&mut self.avatar_texture_cache, IMAGE_CACHE_MAX);
            self.avatar_image_keys.retain(|key| {
                self.image_cache.contains_key(key)
                    || self.avatar_texture_cache.contains_key(key)
            });
            self.image_full_size
                .retain(|key| self.image_cache.contains_key(key));
        }

        while let Ok((url, result)) = self.preview_rx.try_recv() {
            match result {
                Ok(preview) => {
                    if let Some(img_url) = &preview.image_url {
                        if is_safe_public_url(img_url) && !self.image_cache.contains_key(img_url) {
                            self.image_cache.insert(img_url.clone(), ImageState::Loading);
                            let tx = self.image_tx.clone();
                            let img_url_owned = img_url.clone();
                            tokio::spawn(async move {
                                let result = fetch_bounded_image(&img_url_owned).await;
                                let _ = tx.send((img_url_owned, result));
                            });
                        }
                    }
                    self.preview_cache.insert(url, PreviewState::Loaded(preview));
                }
                Err(_) => { self.preview_cache.insert(url, PreviewState::Failed); }
            }
            cap_map(&mut self.preview_cache, PREVIEW_CACHE_MAX);
        }

        // /np drain: send the formatted now-playing message to the active buffer.
        while let Ok((buf_id, np_text)) = self.np_rx.try_recv() {
            if let Some((client, raw_id)) = self.client_for_buffer(&buf_id) {
                client.send_message(&raw_id, &np_text);
            }
        }

        // /sysinfo drain: send to the active buffer as a regular message.
        while let Ok((buf_id, text)) = self.sysinfo_rx.try_recv() {
            if let Some((client, raw_id)) = self.client_for_buffer(&buf_id) {
                client.send_message(&raw_id, &text);
            }
        }

        // File share drain: Matrix bytes stay in Matrix; IRC receives an external URL.
        while let Ok(result) = self.file_share_rx.try_recv() {
            match result {
                Ok(PreparedFileShare::MatrixAttachment {
                    buffer_id,
                    filename,
                    mime,
                    bytes,
                }) => {
                    self.file_share_target_buffer_id = Some(buffer_id.clone());
                    self.file_share_expected_filename = Some(filename.clone());
                    self.file_share_started_at = Some(std::time::Instant::now());
                    self.file_share_status = Some(format!(
                        "Uploading {filename}… do not paste it again"
                    ));
                    if let Err(error) = self.send_matrix_attachment(
                        &buffer_id,
                        &filename,
                        &mime,
                        &bytes,
                    ) {
                        self.finish_file_share_feedback(None);
                        self.file_share_error = Some(error);
                    }
                }
                Ok(PreparedFileShare::ExternalLink { buffer_id, url }) => {
                    if let Some((client, raw_id)) = self.client_for_buffer(&buffer_id) {
                        client.send_message(&raw_id, &url);
                        self.finish_file_share_feedback(Some("✓ File link sent".to_owned()));
                    } else {
                        self.finish_file_share_feedback(None);
                        self.file_share_error = Some(
                            "File link target has no relay connection".to_owned(),
                        );
                    }
                }
                Err(e) if !e.is_empty() => {
                    self.finish_file_share_feedback(None);
                    log::debug!("clipboard/file-share preparation failed: {e}");
                    self.file_share_error = Some(e);
                }
                Err(_) => {
                    self.finish_file_share_feedback(None);
                    self.file_share_target_buffer_id = None;
                }
            }
        }

        if self.file_share_uploading
            && self.file_share_started_at.is_some_and(|started| {
                started.elapsed() >= MATRIX_UPLOAD_CONFIRMATION_TIMEOUT
            })
        {
            self.finish_file_share_feedback(None);
            self.file_share_error = Some(
                "No Matrix upload confirmation arrived. Check the chat before trying again."
                    .to_owned(),
            );
        }

        // Drag-and-drop file upload
        let dropped_files = ctx.input(|i| i.raw.dropped_files.clone());
        if !dropped_files.is_empty() && !self.file_share_uploading {
            if let Some(path) = dropped_files.into_iter().find_map(|f| f.path) {
                let over_thread_panel = self.open_thread_buffer_id.is_some()
                    && ctx.input(|input| {
                        input.pointer.hover_pos().is_some_and(|position| {
                            position.x >= ctx.screen_rect().right() - self.thread_panel_width
                        })
                    });
                let target = if over_thread_panel {
                    self.open_thread_buffer_id.clone()
                } else {
                    self.selected_buffer_id.clone()
                };
                if let Some(buf_id) = target {
                    self.start_file_path(buf_id, path);
                }
            }
        }

        let mut tab_pressed = false;
        let mut arrow_up_shortcut = false;
        let mut arrow_down_shortcut = false;
        let mut history_up = false;
        let mut history_down = false;
        let mut search_shortcut = false;
        let mut jump_next_unread = false;
        let mut jump_buffer_n: Option<usize> = None;
        let mut mention_up = false;
        let mut mention_down = false;
        let mut mention_accept = false;
        let mut mention_cancel = false;
        let mention_open = self.mention_completion.is_some();
        let mut command_up = false;
        let mut command_down = false;
        let mut command_accept = false;
        let mut command_cancel = false;
        let command_open = self.command_completion.is_some();

        ctx.input_mut(|i| {
            if command_open {
                command_accept = i.consume_key(Modifiers::NONE, Key::Tab)
                    || i.consume_key(Modifiers::NONE, Key::Enter);
                command_up = i.consume_key(Modifiers::NONE, Key::ArrowUp);
                command_down = i.consume_key(Modifiers::NONE, Key::ArrowDown);
                command_cancel = i.consume_key(Modifiers::NONE, Key::Escape);
            } else if mention_open {
                mention_accept = i.consume_key(Modifiers::NONE, Key::Tab)
                    || i.consume_key(Modifiers::NONE, Key::Enter);
                mention_up = i.consume_key(Modifiers::NONE, Key::ArrowUp);
                mention_down = i.consume_key(Modifiers::NONE, Key::ArrowDown);
                mention_cancel = i.consume_key(Modifiers::NONE, Key::Escape);
            } else if i.consume_key(Modifiers::NONE, Key::Tab) {
                tab_pressed = true;
            }

            let kb = &self.keybinds;
            if kb.consume(i, crate::ui::keybinds::KeybindAction::CycleBufferUp) {
                arrow_up_shortcut = true;
            }
            if kb.consume(i, crate::ui::keybinds::KeybindAction::CycleBufferDown) {
                arrow_down_shortcut = true;
            }
            if kb.consume(i, crate::ui::keybinds::KeybindAction::ToggleSearch) {
                search_shortcut = true;
            }
            if kb.consume(i, crate::ui::keybinds::KeybindAction::ToggleBufferList) {
                self.show_buffers = !self.show_buffers;
            }
            if kb.consume(i, crate::ui::keybinds::KeybindAction::ToggleNicklist) {
                self.show_nicklist = !self.show_nicklist;
            }
            if kb.consume(i, crate::ui::keybinds::KeybindAction::ToggleToolbar) {
                self.show_toolbar = !self.show_toolbar;
            }
            if kb.consume(i, crate::ui::keybinds::KeybindAction::JumpNextUnread) {
                jump_next_unread = true;
            }
            if let Some(n) = kb.consume_jump_number(i) {
                jump_buffer_n = Some(n);
            }

            let any_mod = i.modifiers.command || i.modifiers.alt || i.modifiers.mac_cmd || i.modifiers.ctrl;
            if !any_mod {
                if i.consume_key(Modifiers::NONE, Key::ArrowUp) { history_up = true; }
                if i.consume_key(Modifiers::NONE, Key::ArrowDown) { history_down = true; }
            }
        });

        if arrow_up_shortcut { self.cycle_buffer(-1); }
        if arrow_down_shortcut { self.cycle_buffer(1); }
        if search_shortcut { self.show_search = !self.show_search; }
        if jump_next_unread { self.jump_next_unread(); }
        if let Some(n) = jump_buffer_n { self.jump_buffer_by_number(n); }

        if self.font_path != self.applied_font_path {
            crate::ui::fonts::apply(ctx, &self.font_path);
            self.applied_font_path = self.font_path.clone();
        }

        let mut style = (*ctx.style()).clone();
        let font_family = if self.use_monospace { FontFamily::Monospace } else { FontFamily::Proportional };

        style.text_styles = [
            (TextStyle::Small, FontId::new(self.font_size * 0.8, font_family.clone())),
            (TextStyle::Body, FontId::new(self.font_size, font_family.clone())),
            (TextStyle::Button, FontId::new(self.font_size, font_family.clone())),
            (TextStyle::Heading, FontId::new(self.font_size * 1.4, font_family.clone())),
            (TextStyle::Monospace, FontId::new(self.font_size, FontFamily::Monospace)),
        ].into();
        style.spacing.item_spacing = Vec2::new(8.0, 4.0);
        style.spacing.window_margin = Margin::same(12.0);
        style.visuals.window_rounding = Rounding::same(12.0);
        style.visuals.widgets.noninteractive.rounding = Rounding::same(8.0);
        style.visuals.widgets.inactive.rounding = Rounding::same(8.0);
        style.visuals.widgets.hovered.rounding = Rounding::same(8.0);
        style.visuals.widgets.active.rounding = Rounding::same(8.0);
        ctx.set_style(style);

        // Effective theme: adaptive override when enabled and loaded, else user's theme.
        let render_theme: crate::ui::theme::AppTheme = self.adaptive_theme_result
            .clone()
            .unwrap_or_else(|| self.theme.clone());

        let accent_color = if render_theme.name == "Default" {
            Color32::from_rgb(100, 149, 237)
        } else {
            Color32::from(render_theme.ansi[4])
        };
        let base_bg = render_theme.background.map(Color32::from).unwrap_or(Color32::from_rgb(18, 18, 18));
        let alpha = (self.opacity * 255.0) as u8;
        let bg_color = Color32::from_rgba_unmultiplied(base_bg.r(), base_bg.g(), base_bg.b(), alpha);

        let luma = 0.299 * base_bg.r() as f32 + 0.587 * base_bg.g() as f32 + 0.114 * base_bg.b() as f32;
        let is_light = luma > 140.0;

        let surface_color = if is_light {
            Color32::from_rgba_unmultiplied(
                base_bg.r().saturating_sub(18),
                base_bg.g().saturating_sub(18),
                base_bg.b().saturating_sub(18),
                alpha,
            )
        } else {
            Color32::from_rgba_unmultiplied(30, 30, 30, alpha)
        };

        let card_bg = if is_light {
            Color32::from_rgba_unmultiplied(
                base_bg.r().saturating_sub(12),
                base_bg.g().saturating_sub(12),
                base_bg.b().saturating_sub(12),
                230,
            )
        } else {
            Color32::from_rgba_unmultiplied(35, 35, 45, 220)
        };

        let text_primary = render_theme.foreground
            .map(Color32::from)
            .unwrap_or_else(|| if is_light { Color32::from_gray(15) } else { Color32::WHITE });
        let text_secondary = if is_light { Color32::from_gray(70)  } else { Color32::from_gray(160) };
        let text_muted     = if is_light { Color32::from_gray(120) } else { Color32::from_gray(100) };

        let border_color = if is_light { Color32::from_gray(200) } else { Color32::from_gray(55) };

        let mut visuals = if is_light { Visuals::light() } else { Visuals::dark() };
        visuals.panel_fill = bg_color;
        visuals.window_fill = surface_color;
        visuals.extreme_bg_color = if is_light {
            Color32::from_rgba_unmultiplied(255, 255, 255, alpha)
        } else {
            Color32::from_rgba_unmultiplied(10, 10, 10, alpha)
        };
        visuals.widgets.active.bg_fill = accent_color;
        visuals.selection.bg_fill = accent_color.linear_multiply(0.5);
        if let Some(fg) = render_theme.foreground {
            visuals.override_text_color = Some(fg.into());
        } else if is_light {
            visuals.override_text_color = Some(text_primary);
        }
        ctx.set_visuals(visuals);

        let mut next_selected_buffer_id = None;
        let mut pending_collapse_toggle: Option<String> = None;
        let mut pending_buffer_command = None;
        let mut next_drag_buffer_id: Option<String> = None;
        let mut pending_mute: Option<(String, String, bool)> = None;
        let mut pending_load_more: Option<(String, String)> = None;
        let mut pending_history_top_rearm: Option<String> = None;
        let mut pending_clear_history_anchor: Option<String> = None;
        let mut consumed_force_scroll_to_bottom = false;
        let mut pending_reply_target: Option<ReplyTarget> = None;
        let mut pending_open_thread_buffer_id: Option<String> = None;

        if self.show_toolbar { egui::TopBottomPanel::top("top_panel")
            .frame(Frame::none().fill(surface_color).inner_margin(Margin::symmetric(12.0, 8.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.visuals_mut().widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;

                    let icon_size = Vec2::splat(24.0);

                    let (rect, res) = ui.allocate_at_least(icon_size, egui::Sense::click());
                    if res.clicked() { self.show_buffers = !self.show_buffers; }
                    let color = if self.show_buffers { accent_color } else { Color32::GRAY };
                    Self::draw_sidebar_icon(ui.painter(), rect, color, false);

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let buf_has_nicklist = self.selected_buffer_id.as_ref()
                            .and_then(|id| self.buffer_by_id(id))
                            .map(|b| b.has_nicklist)
                            .unwrap_or(false);
                        let (rect, res) = ui.allocate_at_least(icon_size, if buf_has_nicklist { egui::Sense::click() } else { egui::Sense::hover() });
                        if buf_has_nicklist && res.clicked() { self.show_nicklist = !self.show_nicklist; }
                        let color = if buf_has_nicklist && self.show_nicklist { accent_color } else { Color32::GRAY.linear_multiply(if buf_has_nicklist { 1.0 } else { 0.4 }) };
                        Self::draw_sidebar_icon(ui.painter(), rect, color, true);
                        ui.add_space(8.0);

                        if ui.button(egui::RichText::new("⚙").size(16.0)).on_hover_text("Settings").clicked() {
                            self.show_settings = !self.show_settings;
                            self.show_connections = false;
                        }
                        if ui.button(egui::RichText::new("🔌").size(14.0)).on_hover_text("Connections").clicked() {
                            self.show_connections = !self.show_connections;
                            self.show_settings = false;
                        }
                        let log_icon = if self.connection_log_unread {
                            egui::RichText::new("⬡").size(16.0).color(Color32::from_rgb(255, 165, 0))
                        } else {
                            egui::RichText::new("⬡").size(16.0)
                        };
                        if ui.button(log_icon).on_hover_text("Connection log").clicked() {
                            self.show_connection_log = !self.show_connection_log;
                            self.connection_log_unread = false;
                            self.show_settings = false;
                            self.show_connections = false;
                        }
                        // Show status indicators for each active connection
                        for conn in &self.connections {
                            if conn.client.is_connected() || conn.connecting_pending {
                                let status_text = if conn.connecting_pending {
                                    format!("● {}", conn.label)
                                } else {
                                    format!("● {}", conn.label)
                                };
                                let status_color = if conn.connecting_pending {
                                    Color32::from_rgb(255, 165, 0)
                                } else {
                                    Color32::from_rgb(50, 205, 50)
                                };
                                ui.label(egui::RichText::new(status_text).color(status_color).small());
                            }
                        }
                    });
                });
            });
        }

        let buf_font_id = FontId::new(
            self.font_size,
            if self.use_monospace {
                FontFamily::Monospace
            } else {
                FontFamily::Proportional
            },
        );
        let replaced_matrix_room_ids = replaced_matrix_room_ids(&self.buffers);
        let longest_buffer_name = self
            .buffers
            .iter()
            .filter(|buffer| {
                buffer_visible_in_sidebar(
                    buffer,
                    self.show_hidden_buffers,
                    &self.collapsed_servers,
                    &replaced_matrix_room_ids,
                )
            })
            .map(|buffer| {
                let avatar_width = if buffer_has_sidebar_avatar(buffer) { 26.0 } else { 0.0 };
                avatar_width + ctx.fonts(|fonts| {
                    fonts
                        .layout_no_wrap(
                            buffer.name.clone(),
                            buf_font_id.clone(),
                            Color32::WHITE,
                        )
                        .size()
                        .x
                })
            })
            .fold(0.0_f32, f32::max);
        let buffers_auto_width = (longest_buffer_name + 88.0)
            .clamp(BUFFERS_MIN_AUTO_WIDTH, BUFFERS_MAX_AUTO_WIDTH);
        self.buffers_width = self.buffers_width.max(buffers_auto_width);
        let responsive_right_kind = if self.open_thread_buffer_id.is_some() {
            RightPanelKind::Thread
        } else if self.show_nicklist
            && self.selected_buffer_id.as_ref()
                .and_then(|id| self.buffer_by_id(id))
                .is_some_and(|buffer| buffer.has_nicklist)
            && self.is_any_connected()
        {
            RightPanelKind::Nicklist
        } else {
            RightPanelKind::None
        };
        let preferred_right_width = match responsive_right_kind {
            RightPanelKind::None => 0.0,
            RightPanelKind::Thread => self.thread_panel_width,
            RightPanelKind::Nicklist => {
                if self.nicklist_width < 80.0 {
                    180.0
                } else {
                    self.nicklist_width
                }
            }
        };
        let responsive_panels = responsive_panel_layout(
            ctx.available_rect().width(),
            self.show_buffers,
            self.buffers_width,
            responsive_right_kind,
            preferred_right_width,
        );

        if responsive_panels.show_buffers {
            let mut pending_sidebar_avatar_loads = Vec::new();
            let buffers_resp = egui::SidePanel::left("buffers_panel")
                .resizable(true)
                .default_width(self.buffers_width.min(responsive_panels.buffers_max_width))
                .min_width(buffers_auto_width.min(responsive_panels.buffers_max_width).max(80.0))
                .max_width(responsive_panels.buffers_max_width)
                .frame(Frame::none().fill(bg_color).inner_margin(Margin::same(10.0)))
                .show(ctx, |ui| {
                    ui.set_clip_rect(ui.max_rect().intersect(ui.clip_rect()));
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("BUFFERS").strong().color(accent_color).size(11.0));
                    ui.add_space(8.0);

                    let is_dragging = self.dragging_buffer_id.is_some();
                    let pointer_pos = ctx.pointer_hover_pos();

                    let dragged_group_ids: HashSet<String> = if let Some(drag_id) = &self.dragging_buffer_id {
                        if let Some(drag_buf) = self.buffer_by_id(drag_id) {
                            if drag_buf.kind == "server" || drag_buf.kind == "core" {
                                let skey = drag_buf.server.clone();
                                self.buffers
                                    .iter()
                                    .filter(|b| b.server == skey && !b.is_matrix_thread())
                                    .map(|b| b.id.clone())
                                    .collect()
                            } else {
                                std::iter::once(drag_id.clone()).collect()
                            }
                        } else { HashSet::new() }
                    } else { HashSet::new() };

                    let mut row_rects: Vec<(egui::Rect, String)> = Vec::new();

                    // Build a lookup from connection prefix → friendly label for headers
                    let conn_label_map: std::collections::HashMap<String, String> = self.connections.iter()
                        .map(|c| (c.prefix.clone(), c.label.clone()))
                        .collect();
                    let multi_conn = conn_label_map.len() > 1;

                    ScrollArea::vertical().show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 2.0;
                        let mut last_conn_prefix: Option<String> = None;
                        for buffer in &self.buffers {
                            if !buffer_visible_in_sidebar(
                                buffer,
                                self.show_hidden_buffers,
                                &self.collapsed_servers,
                                &replaced_matrix_room_ids,
                            ) {
                                continue;
                            }
                            let is_selected = self.selected_buffer_id.as_deref() == Some(&buffer.id);
                            let is_core  = buffer.kind == "core";
                            let is_root  = buffer.kind == "server" || is_core;
                            let is_child = buffer.kind == "channel" || buffer.kind == "private";
                            let show_avatar = buffer_has_sidebar_avatar(buffer);
                            let in_dragged_group = dragged_group_ids.contains(&buffer.id);

                            // Connection header — shown when ≥2 connections are active, but
                            // suppressed when the first buffer in the group is a root buffer
                            // (server/core rows render themselves as section headers).
                            {
                                let buf_conn_prefix = buffer.id.split('/').next().unwrap_or("").to_string();
                                if last_conn_prefix.as_deref() != Some(&buf_conn_prefix) {
                                    if last_conn_prefix.is_some() { ui.add_space(6.0); }
                                    if multi_conn && !is_root {
                                        let header_label = conn_label_map.get(&buf_conn_prefix)
                                            .cloned()
                                            .unwrap_or_else(|| buf_conn_prefix.clone());
                                        ui.label(
                                            egui::RichText::new(header_label.to_uppercase())
                                                .strong()
                                                .color(accent_color)
                                                .size(10.0)
                                        );
                                        ui.add_space(2.0);
                                    }
                                    last_conn_prefix = Some(buf_conn_prefix);
                                }
                            }

                            // Space before each root header row (server/core), except at the
                            // very top (last_conn_prefix was just set, already has add_space).
                            if is_root {
                                ui.add_space(4.0);
                            }

                            let is_muted = buffer.muted;
                            let is_replaced_room = buffer
                                .matrix_room_id
                                .as_ref()
                                .is_some_and(|room_id| replaced_matrix_room_ids.contains(room_id));
                            let (bg, fg) = if is_selected {
                                (accent_color.linear_multiply(0.2), text_primary)
                            } else if is_muted {
                                (Color32::TRANSPARENT, text_muted.linear_multiply(0.6))
                            } else if is_root {
                                // Server/core headers always use accent colour
                                (Color32::TRANSPARENT, accent_color)
                            } else {
                                match buffer.activity {
                                    BufferActivity::Highlight => (Color32::from_rgb(150, 50, 50).linear_multiply(0.3), Color32::from_rgb(255, 100, 100)),
                                    BufferActivity::Message   => (Color32::TRANSPARENT, text_primary),
                                    BufferActivity::Metadata  => (Color32::TRANSPARENT, text_secondary),
                                    BufferActivity::None      => (Color32::TRANSPARENT, text_muted),
                                }
                            };

                            let indent = if is_child { 12.0 } else { 0.0 };

                            // Fixed-height row: derive height from font metrics so the row
                            // never expands beyond one line regardless of text length or font size.
                            let avail_w = ui.available_width();
                            let line_h  = ui.text_style_height(&TextStyle::Body);
                            let row_h   = line_h + 8.0; // 4 px top + 4 px bottom padding

                            // Chevron width reserved on the right for collapsible server groups.
                            let chevron_w = if is_root && !is_core { 18.0_f32 } else { 0.0_f32 };

                            // Allocate exact row space — no sense here so layout is not affected.
                            let (outer_rect, _) = ui.allocate_exact_size(
                                Vec2::new(avail_w, row_h),
                                egui::Sense::hover(),
                            );

                            // Background frame rect (indented for child buffers).
                            // For root headers the frame width leaves room for the chevron.
                            let frame_rect = egui::Rect::from_min_size(
                                egui::pos2(outer_rect.min.x + indent, outer_rect.min.y),
                                egui::vec2((outer_rect.width() - indent).max(0.0), outer_rect.height()),
                            );
                            // Clip background to panel boundary so it never overdraws adjacent panels.
                            let panel_clip  = ui.clip_rect();
                            let fill = if in_dragged_group { bg.linear_multiply(0.35) } else { bg };
                            if fill != Color32::TRANSPARENT {
                                ui.painter().with_clip_rect(frame_rect.intersect(panel_clip))
                                    .rect_filled(frame_rect, Rounding::same(6.0), fill);
                            }

                            // Inner content rect — shrink by chevron on the right for root rows.
                            let inner_rect = egui::Rect::from_min_max(
                                frame_rect.shrink2(Vec2::new(8.0, 4.0)).min,
                                egui::pos2(frame_rect.max.x - chevron_w - 2.0, frame_rect.max.y - 4.0),
                            );
                            let clip_rect  = inner_rect.intersect(panel_clip);

                            if clip_rect.is_positive() {
                                let name = if !is_muted && buffer.activity == BufferActivity::Highlight {
                                    format!("• {}", buffer.name)
                                } else if is_muted {
                                    format!("🔇 {}", buffer.name)
                                } else if is_replaced_room {
                                    format!("↪ {} (replaced)", buffer.name)
                                } else {
                                    buffer.name.clone()
                                };
                                // Root buffers (server + core) use compact header styling.
                                let label = if is_root {
                                    egui::RichText::new(name.to_uppercase()).color(fg).strong().size(10.0)
                                } else if is_muted {
                                    egui::RichText::new(name).color(fg).italics()
                                } else {
                                    egui::RichText::new(name).color(fg).strong()
                                };
                                let unread       = if is_muted || is_root { 0 } else { buffer.unread_count };
                                let buf_activity = buffer.activity;

                                let mut row_ui = ui.child_ui(
                                    inner_rect,
                                    egui::Layout::right_to_left(egui::Align::Center),
                                );
                                row_ui.set_clip_rect(clip_rect);

                                if unread > 0 {
                                    let badge_text = if unread > 99 { "99+".to_string() } else { unread.to_string() };
                                    let badge_bg   = if buf_activity == BufferActivity::Highlight {
                                        Color32::from_rgb(200, 50, 50)
                                    } else {
                                        accent_color
                                    };
                                    Frame::none()
                                        .fill(badge_bg)
                                        .rounding(Rounding::same(8.0))
                                        .inner_margin(Margin::symmetric(4.0, 1.0))
                                        .show(&mut row_ui, |ui| {
                                            ui.label(egui::RichText::new(badge_text)
                                                .color(Color32::WHITE)
                                                .strong()
                                                .size(self.font_size * 0.72));
                                        });
                                }
                                // selectable(false) prevents Label from grabbing clicks
                                // that belong to the row interact registered below.
                                row_ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                    if show_avatar {
                                        let avatar_size = 20.0;
                                        let (avatar_rect, _) = ui.allocate_exact_size(
                                            egui::Vec2::splat(avatar_size),
                                            egui::Sense::hover(),
                                        );
                                        let texture = buffer.matrix_avatar_mxc.as_deref().and_then(|key| {
                                            self.avatar_texture_cache.get(key).or_else(|| match self.image_cache.get(key) {
                                                Some(ImageState::Loaded(texture)) => Some(texture),
                                                _ => None,
                                            })
                                        });
                                        if let Some(texture) = texture {
                                            ui.put(
                                                avatar_rect,
                                                egui::Image::new((texture.id(), egui::Vec2::splat(avatar_size)))
                                                    .rounding(avatar_size / 2.0),
                                            );
                                        } else {
                                            let fallback = sidebar_avatar_fallback_color(
                                                buffer.matrix_room_id.as_deref().unwrap_or(&buffer.full_name),
                                            );
                                            ui.painter().circle_filled(
                                                avatar_rect.center(),
                                                avatar_size / 2.0,
                                                if is_muted { fallback.gamma_multiply(0.55) } else { fallback },
                                            );
                                            ui.painter().text(
                                                avatar_rect.center(),
                                                egui::Align2::CENTER_CENTER,
                                                sidebar_avatar_initial(&buffer.name),
                                                FontId::new(avatar_size * 0.48, FontFamily::Proportional),
                                                Color32::WHITE,
                                            );
                                        }
                                        if let Some(avatar_mxc) = &buffer.matrix_avatar_mxc {
                                            if !self.image_cache.contains_key(avatar_mxc) {
                                                pending_sidebar_avatar_loads.push((buffer.id.clone(), avatar_mxc.clone()));
                                            }
                                        }
                                        ui.add_space(6.0);
                                    }
                                    ui.add(Label::new(label).truncate(true).selectable(false));
                                });
                            }

                            // Row interact rect excludes the chevron column so the two
                            // interactions don't overlap and both fire correctly.
                            let row_interact_rect = if chevron_w > 0.0 {
                                egui::Rect::from_min_max(
                                    outer_rect.min,
                                    egui::pos2(outer_rect.max.x - chevron_w, outer_rect.max.y),
                                )
                            } else {
                                outer_rect
                            };

                            // Register row interaction first (lower hit-test priority than chevron).
                            let row_id = egui::Id::new("buf_row").with(&buffer.id);
                            let resp   = ui.interact(row_interact_rect, row_id, egui::Sense::click_and_drag());

                            // Disclosure toggle — registered after row so it wins the hit-test
                            // when the pointer is inside the (non-overlapping) icon rect. Draw the
                            // triangle ourselves instead of relying on a Unicode chevron glyph:
                            // user-selected fonts often lack it and egui then renders a tofu box.
                            if is_root && !is_core {
                                let chevron_rect = egui::Rect::from_min_max(
                                    egui::pos2(outer_rect.max.x - chevron_w, outer_rect.min.y),
                                    outer_rect.max,
                                );
                                let is_collapsed = self.collapsed_servers.contains(&buffer.server);
                                let chev_id   = egui::Id::new("buf_chev").with(&buffer.id);
                                let chev_resp = ui
                                    .interact(chevron_rect, chev_id, egui::Sense::click())
                                    .on_hover_text(if is_collapsed {
                                        "Expand buffer group"
                                    } else {
                                        "Collapse buffer group"
                                    });
                                let chev_color = if chev_resp.hovered() { text_primary } else { fg };
                                let center = chevron_rect.center();
                                let points = if is_collapsed {
                                    vec![
                                        egui::pos2(center.x - 2.5, center.y - 4.0),
                                        egui::pos2(center.x - 2.5, center.y + 4.0),
                                        egui::pos2(center.x + 3.5, center.y),
                                    ]
                                } else {
                                    vec![
                                        egui::pos2(center.x - 4.0, center.y - 2.5),
                                        egui::pos2(center.x + 4.0, center.y - 2.5),
                                        egui::pos2(center.x, center.y + 3.5),
                                    ]
                                };
                                ui.painter()
                                    .with_clip_rect(chevron_rect.intersect(panel_clip))
                                    .add(egui::Shape::convex_polygon(
                                        points,
                                        chev_color,
                                        egui::Stroke::NONE,
                                    ));
                                if chev_resp.clicked() {
                                    pending_collapse_toggle = Some(buffer.server.clone());
                                }
                            }

                            if resp.hovered() {
                                let cursor = if is_dragging {
                                    egui::CursorIcon::Grabbing
                                } else if ctx.input(|i| i.pointer.primary_down()) {
                                    egui::CursorIcon::Grab
                                } else {
                                    egui::CursorIcon::Default
                                };
                                ctx.set_cursor_icon(cursor);
                            }
                            if resp.drag_started() {
                                next_drag_buffer_id = Some(buffer.id.clone());
                            }
                            if resp.clicked() && !is_dragging {
                                next_selected_buffer_id = Some(buffer.id.clone());
                            }
                            if !is_dragging {
                                let buf_id = buffer.id.clone();
                                let buf_full_name = buffer.full_name.clone();
                                let buf_kind = buffer.kind.clone();
                                let buf_hidden = buffer.hidden;
                                let buf_muted = buffer.muted;
                                resp.context_menu(|ui| {
                                    if buf_muted {
                                        if ui.button("🔔 Unmute Buffer").clicked() {
                                            pending_mute = Some((buf_id.clone(), buf_full_name.clone(), false));
                                            ui.close_menu();
                                        }
                                    } else {
                                        if ui.button("🔇 Mute Buffer").clicked() {
                                            pending_mute = Some((buf_id.clone(), buf_full_name.clone(), true));
                                            ui.close_menu();
                                        }
                                    }
                                    ui.separator();
                                    if buf_kind == "channel" {
                                        if ui.button("Leave Channel").clicked() {
                                            pending_buffer_command = Some((buf_id.clone(), "/part".to_string()));
                                            ui.close_menu();
                                        }
                                    }
                                    if buf_hidden {
                                        if ui.button("Unhide Buffer").clicked() {
                                            pending_buffer_command = Some((buf_id.clone(), "/buffer unhide".to_string()));
                                            ui.close_menu();
                                        }
                                    } else {
                                        if ui.button("Hide Buffer").clicked() {
                                            pending_buffer_command = Some((buf_id.clone(), "/buffer hide".to_string()));
                                            ui.close_menu();
                                        }
                                    }
                                    if ui.button("Close Buffer").clicked() {
                                        pending_buffer_command = Some((buf_id.clone(), "/close".to_string()));
                                        ui.close_menu();
                                    }
                                });
                            }

                            if !in_dragged_group {
                                row_rects.push((outer_rect, buffer.id.clone()));
                            }
                        }
                    });

                    if let Some(id) = next_drag_buffer_id.take() {
                        self.dragging_buffer_id = Some(id);
                    }

                    if self.dragging_buffer_id.is_some() {
                        if let Some(pos) = pointer_pos {
                            let mut drop_before: Option<String> = None;
                            let mut indicator_y: f32 = row_rects.last().map(|(r, _)| r.bottom()).unwrap_or(0.0);

                            for (rect, id) in &row_rects {
                                if pos.y < rect.center().y {
                                    drop_before = Some(id.clone());
                                    indicator_y = rect.top();
                                    break;
                                }
                                indicator_y = rect.bottom();
                            }

                            self.drag_drop_before_id = drop_before;

                            if let Some((first, _)) = row_rects.first() {
                                ui.painter().hline(
                                    first.left()..=first.right(),
                                    indicator_y,
                                    Stroke::new(2.0, accent_color),
                                );
                            }
                        }
                    }

                    if is_dragging && ctx.input(|i| !i.pointer.primary_down()) {
                        if let Some(drag_id) = self.dragging_buffer_id.take() {
                            let drop_id = self.drag_drop_before_id.take();
                            apply_drag_reorder(&mut self.buffers, &drag_id, drop_id.as_deref());
                            self.buffer_order = self
                                .buffers
                                .iter()
                                .filter(|buffer| !buffer.is_matrix_thread())
                                .map(|buffer| buffer.id.clone())
                                .collect();
                        }
                        self.drag_drop_before_id = None;
                    }
                });
            pending_sidebar_avatar_loads.sort();
            pending_sidebar_avatar_loads.dedup();
            for (buffer_id, avatar_mxc) in pending_sidebar_avatar_loads {
                self.avatar_image_keys.insert(avatar_mxc.clone());
                self.ensure_matrix_media_loading(
                    &buffer_id,
                    &MatrixMedia {
                        mxc_uri: avatar_mxc,
                        name: "room-avatar".to_owned(),
                        kind: "image".to_owned(),
                    },
                );
            }
            let w = buffers_resp.response.rect.width();
            if w >= 80.0
                && (!responsive_panels.buffers_constrained
                    || w < responsive_panels.buffers_max_width - 1.0)
            {
                self.buffers_width = w;
            }
            // SidePanel persists its clamped rectangle. Drop only that temporary
            // value so widening the window restores the user's preferred width.
            forget_temporary_panel_width(
                ctx,
                "buffers_panel",
                responsive_panels.buffers_constrained,
            );
        }

        if let Some(id) = next_selected_buffer_id {
            self.select_buffer(id);
        }
        if let Some(server) = pending_collapse_toggle {
            if !self.collapsed_servers.remove(&server) {
                self.collapsed_servers.insert(server);
            }
        }

        if let Some((buf_id, full_name, mute)) = pending_mute {
            if mute {
                self.muted_buffer_names.insert(full_name);
            } else {
                self.muted_buffer_names.remove(&full_name);
            }
            if let Some(b) = self.buffer_by_id_mut(&buf_id) {
                b.muted = mute;
                if mute {
                    b.activity = BufferActivity::None;
                    b.unread_count = 0;
                }
            }
        }

        if let Some((id, cmd)) = pending_buffer_command {
            self.send_command_to_buffer(&id, &cmd);
        }

        let current_buffer_id = self.selected_buffer_id.clone();
        let current_buf = current_buffer_id.as_ref().and_then(|id| self.buffer_by_id(id));
        let current_buffer_nicks = current_buf.map(|b| b.nicks.clone());
        let current_buffer_member_profiles =
            current_buf.map(|buffer| buffer.matrix_member_profiles.clone());
        let current_buffer_mention_candidates =
            current_buf.map(|buffer| buffer.mention_candidates.clone());
        let current_buffer_server = current_buf.map(|buffer| buffer.server.clone());
        let current_buffer_name = current_buf.map(|b| b.name.clone());
        let current_buffer_full_name = current_buf.map(|b| b.full_name.clone());
        let current_matrix_room_id = current_buf.and_then(|b| b.matrix_room_id.clone());
        let current_buffer_is_matrix =
            current_buf.is_some_and(|buffer| buffer.plugin == "matrix");
        let current_buffer_is_replaced = current_buffer_id
            .as_deref()
            .is_some_and(|buffer_id| replacement_buffer_id(&self.buffers, buffer_id).is_some());
        let current_buffer_mention_aliases = current_buf
            .map(Buffer::own_mention_aliases)
            .unwrap_or_default();
        let (current_buffer_messages, inherited_history_line_ids) = current_buffer_id
            .as_deref()
            .map(|buffer_id| composed_upgrade_history(&self.buffers, buffer_id))
            .map(|(messages, inherited)| (Some(messages), inherited))
            .unwrap_or_else(|| (None, HashSet::new()));
        let current_history_load_buffer_id = current_buffer_id.as_deref().and_then(|buffer_id| {
            upgrade_history_load_buffer_id(
                &self.buffers,
                buffer_id,
                &self.history_exhausted_buffer_ids,
            )
        });
        let current_history_exhausted = current_buffer_id
            .as_deref()
            .is_some_and(|buffer_id| self.history_exhausted_buffer_ids.contains(buffer_id));
        let _current_buffer_last_read_id = current_buf.and_then(|b| b.last_read_id.clone());
        let current_buffer_visit_marker_id = current_buf.and_then(|b| b.visit_start_marker_id.clone());
        let current_buffer_topic = current_buf.map(|b| b.topic.clone()).unwrap_or_default();
        let current_buffer_modes = current_buf.map(|b| b.modes.clone()).unwrap_or_default();
        let current_buffer_kind = current_buf.map(|b| b.kind.clone()).unwrap_or_default();
        let thread_summaries: HashMap<String, (String, usize, bool)> = current_matrix_room_id
            .as_ref()
            .map(|room_id| {
                self.buffers
                    .iter()
                    .filter(|buffer| {
                        buffer.is_matrix_thread()
                            && buffer.matrix_room_id.as_deref() == Some(room_id.as_str())
                    })
                    .filter_map(|buffer| {
                        buffer.matrix_thread_root.as_ref().map(|root| {
                            (
                                root.clone(),
                                (
                                    buffer.id.clone(),
                                    group_thread_lines(&buffer.messages)
                                        .len()
                                        .saturating_sub(1),
                                    buffer.activity != BufferActivity::None,
                                ),
                            )
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let font_id = FontId::new(self.font_size, if self.use_monospace { FontFamily::Monospace } else { FontFamily::Proportional });

        let any_connected = self.is_any_connected();

        let current_buf_has_nicklist = current_buf.map(|b| b.has_nicklist).unwrap_or(false);
        let thread_room_changed = self
            .open_thread_snapshot
            .as_ref()
            .is_some_and(|thread| thread.matrix_room_id != current_matrix_room_id);
        if thread_room_changed {
            self.close_thread();
        }
        if self.open_thread_buffer_id.is_some() {
            if let Some(live_id) = live_thread_buffer_id(
                &self.buffers,
                self.open_thread_buffer_id.as_deref(),
                self.open_thread_snapshot.as_ref(),
            ) {
                self.open_thread_buffer_id = Some(live_id);
            }
        }
        if let Some(thread_id) = self.open_thread_buffer_id.as_deref() {
            let current_thread = self
                .buffer_by_id(thread_id)
                .filter(|buffer| {
                    buffer.is_matrix_thread()
                        && buffer.matrix_room_id == current_matrix_room_id
                })
                .cloned();
            self.open_thread_snapshot =
                stable_thread_snapshot(current_thread.as_ref(), self.open_thread_snapshot.as_ref());
        }
        let open_thread = self.open_thread_snapshot.clone();

        if let Some(thread) = open_thread {
            let thread_messages = group_thread_lines(&thread.messages);
            let room_label = current_buffer_name
                .clone()
                .unwrap_or_else(|| "Matrix".to_owned());
            let thread_width = responsive_panels.right_width;
            let panel = egui::SidePanel::right("thread_panel")
                .resizable(true)
                .default_width(thread_width)
                .min_width(responsive_panels.right_min_width)
                .max_width(responsive_panels.right_max_width)
                .frame(
                    Frame::none()
                        .fill(surface_color)
                        .inner_margin(Margin::same(12.0))
                        .stroke(Stroke::new(1.0, border_color)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new("Thread")
                                    .strong()
                                    .size(self.font_size + 2.0),
                            );
                            ui.label(
                                egui::RichText::new(&room_label)
                                    .small()
                                    .color(text_muted),
                            );
                        });
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui
                                    .button(egui::RichText::new("×").size(17.0))
                                    .on_hover_text("Close thread")
                                    .clicked()
                                {
                                    self.close_thread();
                                }
                            },
                        );
                    });
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

                    let list_height = (ui.available_height() - 58.0).max(100.0);
                    ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .auto_shrink([false, false])
                        .max_height(list_height)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            if thread_messages.is_empty() {
                                ui.vertical_centered(|ui| {
                                    ui.add_space(28.0);
                                    ui.label(
                                        egui::RichText::new("No thread messages yet")
                                            .color(text_muted),
                                    );
                                });
                            }
                            for (index, block) in thread_messages.iter().enumerate() {
                                let background_shape =
                                    ui.painter().add(egui::Shape::Noop);
                                let block_response = Frame::none()
                                    .rounding(Rounding::same(7.0))
                                    .inner_margin(Margin::symmetric(9.0, 7.0))
                                    .show(ui, |ui| {
                                        if index == 0 {
                                            ui.label(
                                                egui::RichText::new("ORIGINAL MESSAGE")
                                                    .small()
                                                    .strong()
                                                    .color(accent_color),
                                            );
                                            ui.add_space(3.0);
                                        }
                                        ui.horizontal_wrapped(|ui| {
                                            let mut avatar_rect: Option<Rect> = None;
                                            if let Some(profiles) =
                                                current_buffer_member_profiles.as_deref()
                                            {
                                                let avatar_response = self.render_matrix_avatar(
                                                    ui,
                                                    &thread.id,
                                                    profiles,
                                                    &block.prefix,
                                                    24.0,
                                                    accent_color,
                                                    Color32::WHITE,
                                                );
                                                avatar_rect = Some(avatar_response.rect);
                                            }
                                            let author_sections = ANSIParser::parse(&block.prefix);
                                            let plain_author: String = author_sections
                                                .iter()
                                                .map(|section| section.text.as_str())
                                                .collect();
                                            let display_author_sections =
                                                compact_matrix_prefix_sections(
                                                    &plain_author,
                                                    &author_sections,
                                                );
                                            let prefix_response = ui.scope(|ui| {
                                                ui.spacing_mut().item_spacing.x = 0.0;
                                                for section in display_author_sections {
                                                    let format = section
                                                        .style
                                                        .to_format(font_id.clone(), &render_theme);
                                                    self.render_text_with_emoji(
                                                        ui,
                                                        &section.text,
                                                        &format,
                                                        false,
                                                        true,
                                                    );
                                                }
                                            }).response;
                                            let author_card = message_sender_profile_card(
                                                &thread.id,
                                                &block.prefix,
                                                current_buffer_server
                                                    .as_deref()
                                                    .unwrap_or_default(),
                                                true,
                                                current_buffer_member_profiles
                                                    .as_deref()
                                                    .unwrap_or_default(),
                                                current_buffer_nicks
                                                    .as_deref()
                                                    .unwrap_or_default(),
                                            );
                                            if let (Some(rect), Some(card)) =
                                                (avatar_rect, author_card.clone())
                                            {
                                                self.open_profile_on_author_click(
                                                    ui,
                                                    ui.make_persistent_id((
                                                        "thread_author_profile",
                                                        "avatar",
                                                        &thread.id,
                                                        block.matrix_event_id.as_deref(),
                                                        index,
                                                    )),
                                                    rect,
                                                    Some(card),
                                                );
                                            }
                                            self.open_profile_on_author_click(
                                                ui,
                                                ui.make_persistent_id((
                                                    "thread_author_profile",
                                                    "name",
                                                    &thread.id,
                                                    block.matrix_event_id.as_deref(),
                                                    index,
                                                )),
                                                prefix_response.rect,
                                                author_card,
                                            );
                                            ui.label(
                                                egui::RichText::new(
                                                    block.timestamp
                                                        .with_timezone(&chrono::Local)
                                                        .format("%H:%M")
                                                        .to_string(),
                                                )
                                                .small()
                                                .color(text_muted),
                                            );
                                        });
                                        if let Some(reply) = &block.reply {
                                            ui.add_space(2.0);
                                            render_reply_context_card(
                                                ui,
                                                reply,
                                                card_bg,
                                                accent_color,
                                                text_secondary,
                                            );
                                            ui.add_space(2.0);
                                        }
                                        for content in &block.content {
                                            if content.message.is_empty() && content.media.is_none() {
                                                ui.add_space(4.0);
                                                continue;
                                            }
                                            let previews = self.render_message_content(
                                                ui,
                                                &content.message,
                                                content.media.as_ref(),
                                                Some(&thread.id),
                                                &font_id,
                                                &render_theme,
                                            );
                                            self.render_message_previews(
                                                ui,
                                                &previews,
                                                text_secondary,
                                                text_muted,
                                                card_bg,
                                                border_color,
                                                accent_color,
                                            );
                                        }
                                    });
                                let thread_interactable = block_response
                                    .response
                                    .interact(egui::Sense::click());
                                ui.painter().set(
                                    background_shape,
                                    egui::Shape::Vec(message_row_shapes(
                                        thread_interactable.rect,
                                        index == 0,
                                        thread_interactable.contains_pointer(),
                                        accent_color,
                                        text_primary,
                                    )),
                                );
                                thread_interactable.context_menu(|ui| {
                                    ui.set_min_width(170.0);
                                    let delete_enabled = block.matrix_event_id.is_some();
                                    if ui
                                        .add_enabled(
                                            delete_enabled,
                                            egui::Button::new("Delete message…"),
                                        )
                                        .on_disabled_hover_text(
                                            "This line has no Matrix event ID",
                                        )
                                        .clicked()
                                    {
                                        self.pending_redaction_error = None;
                                        self.pending_redaction = block.matrix_event_id.as_ref().map(
                                            |matrix_event_id| RedactionTarget {
                                                buffer_id: thread.id.clone(),
                                                matrix_event_id: matrix_event_id.clone(),
                                                sender: block.prefix.clone(),
                                                message: block.content.iter().find_map(|content| {
                                                    content.media.as_ref().map(|media| media.name.clone())
                                                }).or_else(|| {
                                                    block.content.iter()
                                                        .map(|content| content.message.as_str())
                                                        .find(|message| !message.is_empty())
                                                        .map(ToOwned::to_owned)
                                                }).unwrap_or_else(|| "attachment".to_owned()),
                                            },
                                        );
                                        ui.close_menu();
                                    }
                                });
                                ui.add_space(3.0);
                            }
                        });
                    ui.separator();
                    ui.add_space(5.0);
                    let thread_owns_file_share = self.file_share_target_buffer_id.as_deref()
                        == self.open_thread_buffer_id.as_deref();
                    if thread_owns_file_share {
                        if let Some(status) = self.file_share_status.clone() {
                            ui.horizontal_wrapped(|ui| {
                                if self.file_share_uploading {
                                    ui.spinner();
                                }
                                ui.label(
                                    egui::RichText::new(status)
                                        .color(accent_color)
                                        .small(),
                                );
                            });
                            ui.add_space(3.0);
                        }
                    }
                    if thread_owns_file_share {
                        if let Some(err) = self.file_share_error.clone() {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new(format!("⚠ {err}"))
                                    .color(Color32::from_rgb(220, 80, 80))
                                    .small(),
                            );
                            if ui.small_button("x").clicked() {
                                self.file_share_error = None;
                            }
                        });
                        ui.add_space(3.0);
                        }
                    }
                    ui.horizontal(|ui| {
                        let attach_enabled = !self.file_share_uploading;
                        let attach_label = if self.file_share_uploading {
                            egui::RichText::new("⏳").size(16.0).color(Color32::GRAY)
                        } else {
                            egui::RichText::new("📎").size(16.0)
                        };
                        if ui
                            .add_enabled(
                                attach_enabled,
                                egui::Button::new(attach_label).frame(false),
                            )
                            .on_hover_text("Attach a file to this Matrix thread")
                            .clicked()
                        {
                            if let Some(buffer_id) = self.open_thread_buffer_id.clone() {
                                self.start_file_picker(buffer_id);
                            }
                        }
                        let input_id = ui.make_persistent_id("thread_composer_input");
                        let selection_before_click = crate::ui::input::input_selection(
                            ui.ctx(),
                            input_id,
                            &self.thread_input_text,
                        );
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.thread_input_text)
                                .id(input_id)
                                .hint_text("Reply in thread…")
                                .margin(Margin::symmetric(8.0, 5.0))
                                .desired_width((ui.available_width() - 86.0).max(40.0)),
                        );
                        let context_paste_image = crate::ui::input::input_context_menu(
                            &response,
                            &mut self.thread_input_text,
                            selection_before_click,
                        );
                        let paste_shortcut = response.has_focus() && paste_shortcut_this_frame;
                        let paste_image = context_paste_image
                            || (response.has_focus() && paste_image_this_frame);
                        if paste_shortcut && !paste_image {
                            self.file_share_error = None;
                        } else if paste_image {
                            if let Some(buffer_id) = clipboard_upload_target(
                                true,
                                self.open_thread_buffer_id.as_deref(),
                                current_buffer_id.as_deref(),
                            ) {
                                self.start_matrix_clipboard_upload(buffer_id, ctx);
                            } else {
                                self.file_share_error = Some(
                                    "Cannot paste: this thread is not ready for attachments"
                                        .to_owned(),
                                );
                            }
                        }
                        if std::mem::take(&mut self.focus_thread_input) {
                            response.request_focus();
                        }
                        let enter = response.lost_focus()
                            && ctx.input(|input| input.key_pressed(egui::Key::Enter));
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Send")
                                        .color(Color32::WHITE)
                                        .strong(),
                                )
                                .fill(accent_color),
                            )
                            .clicked()
                            || enter
                        {
                            self.send_thread_message();
                            self.focus_thread_input = true;
                        }
                    });
                });
            let width = panel.response.rect.width();
            if !responsive_panels.right_constrained
                || width < responsive_panels.right_max_width - 1.0
            {
                self.thread_panel_width = width;
            }
            forget_temporary_panel_width(
                ctx,
                "thread_panel",
                responsive_panels.right_constrained,
            );
        } else if responsive_panels.show_right
            && self.show_nicklist
            && current_buf_has_nicklist
            && any_connected
            && current_buffer_id.is_some()
        {
            if self.nicklist_width < 80.0 {
                self.nicklist_width = 180.0;
            }
            let nicks_resp = egui::SidePanel::right("nicks_panel_2")
                .resizable(true)
                .default_width(responsive_panels.right_width)
                .min_width(responsive_panels.right_min_width)
                .max_width(responsive_panels.right_max_width)
                .frame(Frame::none().fill(bg_color).inner_margin(Margin::same(10.0)))
                .show(ctx, |ui| {
                    ui.set_clip_rect(ui.max_rect().intersect(ui.clip_rect()));
                    // Force content to fill the full panel width so that egui's
                    // PanelState stores the actual panel width (not just content
                    // min_rect), preventing the panel from snapping back to
                    // content width after every resize.
                    ui.set_min_width(ui.available_width());
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("NICKS").strong().color(accent_color).size(11.0));
                    ui.add_space(8.0);
                    ScrollArea::vertical().show(ui, |ui| {
                            if let Some(nicks) = &current_buffer_nicks {
                                let clusters = if current_buffer_is_matrix {
                                    matrix_nick_clusters(nicks)
                                } else {
                                    nicks
                                        .iter()
                                        .cloned()
                                        .map(|nick| MatrixNickCluster {
                                            display_name: nick.name.clone(),
                                            members: vec![nick],
                                            user_ids: Vec::new(),
                                        })
                                        .collect()
                                };
                                for cluster in clusters {
                                    let nick = &cluster.members[0];
                                    let text = format!("{}{}", nick.prefix, cluster.display_name);
                                    let input = if nick.away {
                                        text.clone()
                                    } else if self.colored_nicks {
                                        if render_theme.name == "Default" { format!("{}{}", nick.color_ansi, text) }
                                        else {
                                            let idx = Self::hash_nick(&cluster.display_name);
                                            let esc = if idx < 8 { format!("\x1B[{}m", 30 + idx) } else { format!("\x1B[{}m", 90 + idx - 8) };
                                            format!("{}{}", esc, text)
                                        }
                                    } else { text };
                                    let sections = ANSIParser::parse(&input);
                                    let label_res = ui.horizontal(|ui| {
                                        if current_buffer_is_matrix {
                                            if let (Some(buffer_id), Some(profiles)) = (
                                                current_buffer_id.as_deref(),
                                                current_buffer_member_profiles.as_deref(),
                                            ) {
                                                let avatar_nick = cluster
                                                    .members
                                                    .iter()
                                                    .find(|member| {
                                                        matrix_profile_for_nick(profiles, &member.name)
                                                            .and_then(|profile| profile.avatar_mxc)
                                                            .is_some()
                                                    })
                                                    .map(|member| member.name.as_str())
                                                    .unwrap_or(cluster.display_name.as_str());
                                                self.render_matrix_avatar(
                                                    ui,
                                                    buffer_id,
                                                    profiles,
                                                    avatar_nick,
                                                    22.0,
                                                    accent_color,
                                                    Color32::WHITE,
                                                );
                                            }
                                        }
                                        ui.spacing_mut().item_spacing.x = 4.0;
                                        for s in sections {
                                            let mut fmt = s.style.to_format(font_id.clone(), &render_theme);
                                            if nick.away {
                                                fmt.color = text_muted;
                                                fmt.italics = true;
                                            }
                                            self.render_text_with_emoji(ui, &s.text, &fmt, false, true);
                                        }
                                        if let Some(user_id) = cluster.user_ids.first() {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "·{}",
                                                    matrix_identity_disambiguator(user_id)
                                                ))
                                                .small()
                                                .color(text_muted),
                                            )
                                            .on_hover_text(user_id);
                                        }
                                    }).response;
                                    let label_res = label_res.interact(egui::Sense::click());
                                    if response_primary_clicked(ui, &label_res) {
                                        let matrix_identities: Vec<MatrixProfileIdentity> = cluster
                                            .members
                                            .iter()
                                            .zip(&cluster.user_ids)
                                            .map(|(member, user_id)| MatrixProfileIdentity {
                                                user_id: user_id.clone(),
                                                profile: current_buffer_member_profiles
                                                    .as_deref()
                                                    .and_then(|profiles| {
                                                        matrix_profile_for_nick(
                                                            profiles,
                                                            &member.name,
                                                        )
                                                    }),
                                            })
                                            .collect();
                                        let matrix = matrix_identities
                                            .iter()
                                            .filter_map(|identity| identity.profile.as_ref())
                                            .find(|profile| profile.avatar_mxc.is_some())
                                            .or_else(|| {
                                                matrix_identities
                                                    .iter()
                                                    .find_map(|identity| identity.profile.as_ref())
                                            })
                                            .cloned()
                                            .or_else(|| {
                                            current_buffer_member_profiles
                                                .as_deref()
                                                .and_then(|profiles| {
                                                    matrix_profile_for_nick(profiles, &nick.name)
                                                })
                                            });
                                        self.profile_card = Some(UserProfileCard {
                                            buffer_id: current_buffer_id.clone().unwrap_or_default(),
                                            nick: cluster.display_name.clone(),
                                            prefix: nick.prefix.clone(),
                                            server: current_buffer_server.clone().unwrap_or_default(),
                                            is_matrix: current_buffer_is_matrix,
                                            matrix_user_id: cluster.user_ids.first().cloned().or_else(|| {
                                                current_buffer_mention_candidates
                                                    .as_deref()
                                                    .and_then(|candidates| {
                                                        matrix_user_id_for_nick(candidates, &nick.name)
                                                    })
                                            }),
                                            matrix,
                                            matrix_identities,
                                        });
                                    }
                                    label_res.context_menu(|ui| {
                                        if let Some(user_id) = cluster.user_ids.first() {
                                            if ui.button("Message").clicked() {
                                                self.send_command(&format!("/query {}", user_id));
                                                ui.close_menu();
                                            }
                                        } else {
                                            if ui.button(format!("Query {}", nick.name)).clicked() {
                                                self.send_command(&format!("/query {}", nick.name));
                                                ui.close_menu();
                                            }
                                        }
                                        if cluster.user_ids.is_empty() {
                                            if ui.button(format!("Whois {}", nick.name)).clicked() {
                                                self.send_command(&format!("/whois {}", nick.name));
                                                ui.close_menu();
                                            }
                                        }
                                    });
                                }
                            }
                        });
                });
            let width = nicks_resp.response.rect.width();
            if !responsive_panels.right_constrained
                || width < responsive_panels.right_max_width - 1.0
            {
                self.nicklist_width = width;
            }
            forget_temporary_panel_width(
                ctx,
                "nicks_panel_2",
                responsive_panels.right_constrained,
            );
        }

        if let Some(card) = self.profile_card.clone() {
            if let Some(avatar_mxc) = card
                .matrix
                .as_ref()
                .and_then(|profile| profile.avatar_mxc.as_ref())
            {
                self.ensure_matrix_media_loading(
                    &card.buffer_id,
                    &MatrixMedia {
                        mxc_uri: avatar_mxc.clone(),
                        name: "member-avatar".to_owned(),
                        kind: "image".to_owned(),
                    },
                );
            }

            let mut open = true;
            let mut mention = false;
            let mut mention_identity = None;
            let mut query_user_id = None;
            let mut query = false;
            let mut whois = false;
            egui::Window::new("Profile")
                .id(egui::Id::new("nick_profile_card"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(420.0)
                .max_width(480.0)
                .anchor(egui::Align2::RIGHT_CENTER, [-24.0, 0.0])
                .show(ctx, |ui| {
                    let display_name = card
                        .matrix
                        .as_ref()
                        .map(|profile| profile.display_name.as_str())
                        .unwrap_or(card.nick.as_str());
                    ui.vertical_centered(|ui| {
                        let avatar_mxc = card
                            .matrix
                            .as_ref()
                            .and_then(|profile| profile.avatar_mxc.as_deref());
                        if let Some(ImageState::Loaded(texture)) =
                            avatar_mxc.and_then(|key| self.image_cache.get(key))
                        {
                            ui.add(
                                egui::Image::new((texture.id(), egui::vec2(96.0, 96.0)))
                                    .rounding(48.0),
                            );
                        } else {
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(96.0, 96.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().circle_filled(
                                rect.center(),
                                48.0,
                                accent_color.gamma_multiply(0.45),
                            );
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                display_name
                                    .chars()
                                    .find(|character| character.is_alphanumeric())
                                    .map(|character| character.to_uppercase().to_string())
                                    .unwrap_or_else(|| "?".to_owned()),
                                FontId::new(36.0, FontFamily::Proportional),
                                Color32::WHITE,
                            );
                            if avatar_mxc.is_some() {
                                ui.spinner();
                            }
                        }
                        ui.add_space(8.0);
                        self.render_profile_identity(
                            ui,
                            display_name,
                            FontId::new(22.0, FontFamily::Proportional),
                            text_primary,
                        );
                    });

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(6.0);
                    if card.matrix_identities.len() > 1 {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} distinct identities",
                                card.matrix_identities.len()
                            ))
                            .strong(),
                        );
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical().max_height(250.0).show(ui, |ui| {
                            for (index, identity) in card.matrix_identities.iter().enumerate() {
                                if index > 0 {
                                    ui.add_space(4.0);
                                    ui.separator();
                                    ui.add_space(4.0);
                                }
                                ui.label(
                                    egui::RichText::new(matrix_identity_source(&identity.user_id))
                                        .small()
                                        .color(accent_color),
                                );
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(egui::RichText::new(&identity.user_id).monospace());
                                    if ui.small_button("Copy").clicked() {
                                        ui.output_mut(|output| {
                                            output.copied_text = identity.user_id.clone();
                                        });
                                    }
                                    if ui.small_button("@ Mention").clicked() {
                                        mention_identity = Some((
                                            identity
                                                .profile
                                                .as_ref()
                                                .map(|profile| profile.display_name.clone())
                                                .unwrap_or_else(|| card.nick.clone()),
                                            identity.user_id.clone(),
                                        ));
                                    }
                                    if ui.small_button("Message").clicked() {
                                        query_user_id = Some(identity.user_id.clone());
                                    }
                                });
                                if let Some(profile) = &identity.profile {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} · {} · power {}",
                                            profile.role,
                                            profile.membership,
                                            profile
                                                .power_level
                                                .map(|power| power.to_string())
                                                .unwrap_or_else(|| "—".to_owned())
                                        ))
                                        .small()
                                        .color(text_muted),
                                    );
                                } else {
                                    ui.label(
                                        egui::RichText::new("Profile details not loaded")
                                            .small()
                                            .color(text_muted),
                                    );
                                }
                            }
                        });
                    } else if let Some(profile) = &card.matrix {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new(&profile.user_id).monospace());
                            if ui.small_button("Copy").clicked() {
                                ui.output_mut(|output| {
                                    output.copied_text = profile.user_id.clone();
                                });
                            }
                        });
                        egui::Grid::new("matrix_profile_details")
                            .num_columns(2)
                            .spacing([12.0, 5.0])
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new("Role").color(text_muted));
                                ui.label(&profile.role);
                                ui.end_row();
                                ui.label(egui::RichText::new("Membership").color(text_muted));
                                ui.label(&profile.membership);
                                ui.end_row();
                                if let Some(power_level) = profile.power_level {
                                    ui.label(egui::RichText::new("Power level").color(text_muted));
                                    ui.label(power_level.to_string());
                                    ui.end_row();
                                }
                                ui.label(egui::RichText::new("Room nick").color(text_muted));
                                self.render_profile_identity(
                                    ui,
                                    &profile.nick,
                                    FontId::new(self.font_size, FontFamily::Monospace),
                                    text_primary,
                                );
                                ui.end_row();
                            });
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if ui
                                .button(egui::RichText::new("@ Mention").color(accent_color))
                                .clicked()
                            {
                                mention = true;
                            }
                            if ui
                                .button(egui::RichText::new("Message").color(accent_color))
                                .clicked()
                            {
                                query_user_id = matrix_profile_query_target(&card);
                            }
                        });
                    } else if card.is_matrix {
                        if let Some(user_id) = &card.matrix_user_id {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(egui::RichText::new(user_id).monospace());
                                if ui.small_button("Copy").clicked() {
                                    ui.output_mut(|output| {
                                        output.copied_text = user_id.clone();
                                    });
                                }
                            });
                        }
                        ui.label(egui::RichText::new("Matrix member").color(text_muted));
                        if card.matrix_user_id.is_some()
                            && ui
                                .button(egui::RichText::new("@ Mention").color(accent_color))
                                .clicked()
                        {
                            mention = true;
                        }
                        if card.matrix_user_id.is_some()
                            && ui
                                .button(egui::RichText::new("Message").color(accent_color))
                                .clicked()
                        {
                            query_user_id = matrix_profile_query_target(&card);
                        }
                    } else {
                        egui::Grid::new("irc_profile_details")
                            .num_columns(2)
                            .spacing([12.0, 5.0])
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new("Nick").color(text_muted));
                                self.render_profile_identity(
                                    ui,
                                    &format!("{}{}", card.prefix, card.nick),
                                    FontId::new(self.font_size, FontFamily::Monospace),
                                    text_primary,
                                );
                                ui.end_row();
                                ui.label(egui::RichText::new("Network").color(text_muted));
                                ui.label(&card.server);
                                ui.end_row();
                            });
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            query = ui.button("Message").clicked();
                            whois = ui.button("Whois").clicked();
                        });
                    }
                });
            if !open {
                self.profile_card = None;
            } else if let Some((display_name, user_id)) = mention_identity {
                let label = if display_name.starts_with('@') {
                    display_name
                } else {
                    format!("@{display_name}")
                };
                if !self.input_text.is_empty()
                    && !self
                        .input_text
                        .chars()
                        .last()
                        .is_some_and(char::is_whitespace)
                {
                    self.input_text.push(' ');
                }
                self.input_text.push_str(&label);
                self.input_text.push(' ');
                self.selected_mentions.retain(|selected| {
                    selected.user_id != user_id && selected.label != label
                });
                self.selected_mentions.push(SelectedMention { label, user_id });
                self.focus_input = true;
            } else if mention {
                if let Some(user_id) = card
                    .matrix
                    .as_ref()
                    .map(|profile| profile.user_id.clone())
                    .or_else(|| card.matrix_user_id.clone())
                {
                    let display_name = card
                        .matrix
                        .as_ref()
                        .map(|profile| profile.display_name.as_str())
                        .unwrap_or(card.nick.as_str());
                    let label = if display_name.starts_with('@') {
                        display_name.to_owned()
                    } else {
                        format!("@{display_name}")
                    };
                    if !self.input_text.is_empty()
                        && !self
                            .input_text
                            .chars()
                            .last()
                            .is_some_and(char::is_whitespace)
                    {
                        self.input_text.push(' ');
                    }
                    self.input_text.push_str(&label);
                    self.input_text.push(' ');
                    self.selected_mentions.retain(|selected| {
                        selected.user_id != user_id && selected.label != label
                    });
                    self.selected_mentions.push(SelectedMention {
                        label,
                        user_id,
                    });
                    self.focus_input = true;
                }
            } else if let Some(user_id) = query_user_id {
                self.send_command(&format!("/query {}", user_id));
            } else if query {
                self.send_command(&format!("/query {}", card.nick));
            } else if whois {
                self.send_command(&format!("/whois {}", card.nick));
            }
        }

        if current_buffer_id.is_some() {
            // Is the SELECTED buffer's connection up? Different from any_connected when
            // multiple connections are configured and only some are alive.
            let selected_buffer_connected = current_buffer_id.as_ref()
                .and_then(|id| id.split('/').next())
                .and_then(|prefix| self.connections.iter().find(|c| c.prefix == prefix))
                .map(|c| c.client.is_connected())
                .unwrap_or(false);
            let selected_buffer_is_matrix = current_buffer_id
                .as_deref()
                .is_some_and(|buffer_id| self.is_matrix_buffer(buffer_id));

            egui::TopBottomPanel::bottom("input_panel")
                .frame(Frame::none().fill(surface_color).inner_margin(Margin::symmetric(16.0, 10.0)))
                .show(ctx, |ui| {
                    if let Some(reply) = self.reply_target.clone().filter(|reply| {
                        current_buffer_id.as_deref() == Some(reply.buffer_id.as_str())
                    }) {
                        let mut preview: String = reply.message.chars().take(120).collect();
                        if reply.message.chars().count() > 120 {
                            preview.push('…');
                        }
                        ui.horizontal_wrapped(|ui| {
                            let display_sender = compact_matrix_sender_label(&reply.sender);
                            ui.label(
                                egui::RichText::new(format!("Replying to {display_sender}"))
                                    .strong()
                                    .color(accent_color),
                            );
                            ui.label(
                                egui::RichText::new(preview)
                                    .color(text_secondary)
                                    .italics(),
                            );
                            if ui.small_button("x").on_hover_text("Cancel reply").clicked() {
                                self.reply_target = None;
                            }
                        });
                        ui.add_space(4.0);
                    }
                    // Attachment state belongs beside the composer that owns its immutable target.
                    let room_owns_file_share = self.file_share_target_buffer_id.is_none()
                        || self.file_share_target_buffer_id.as_deref()
                            == self.selected_buffer_id.as_deref();
                    if room_owns_file_share {
                        if let Some(status) = self.file_share_status.clone() {
                            ui.horizontal_wrapped(|ui| {
                                if self.file_share_uploading {
                                    ui.spinner();
                                }
                                ui.label(
                                    egui::RichText::new(status)
                                        .color(accent_color)
                                        .small(),
                                );
                                if !self.file_share_uploading
                                    && ui.small_button("x").clicked()
                                {
                                    self.file_share_status = None;
                                }
                            });
                        }
                        if let Some(err) = self.file_share_error.clone() {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(egui::RichText::new(format!("⚠ {err}"))
                                    .color(Color32::from_rgb(220, 80, 80)).small());
                                if ui.small_button("x").clicked() {
                                    self.file_share_error = None;
                                }
                            });
                        }
                    }
                    ui.horizontal(|ui| {
                        let hint = if !selected_buffer_connected {
                            "Disconnected — reconnect before sending"
                        } else if current_buffer_kind == "server" {
                            "Type /join #channel or any IRC command..."
                        } else {
                            "Type a message..."
                        };

                        // 📎 Matrix native attachment / IRC file-link button
                        let attach_enabled = selected_buffer_connected && !self.file_share_uploading;
                        let attach_label = if self.file_share_uploading {
                            egui::RichText::new("⏳").size(16.0).color(Color32::GRAY)
                        } else {
                            egui::RichText::new("📎").size(16.0)
                        };
                        if ui.add_enabled(
                            attach_enabled,
                            egui::Button::new(attach_label).frame(false),
                        ).on_hover_text(
                            if selected_buffer_is_matrix {
                                "Attach a file directly to this Matrix room"
                            } else {
                                "Share a file link via files.interdo.me"
                            },
                        ).clicked() {
                            if let Some(buf_id) = self.selected_buffer_id.clone() {
                                self.start_file_picker(buf_id);
                            }
                        }

                        ui.add_enabled_ui(selected_buffer_connected, |ui| {
                            let input_id = ui.make_persistent_id("room_composer_input");
                            let selection_before_click = crate::ui::input::input_selection(
                                ui.ctx(),
                                input_id,
                                &self.input_text,
                            );
                            let text_edit = egui::TextEdit::singleline(&mut self.input_text)
                                .id(input_id)
                                .hint_text(hint)
                                .margin(Margin::symmetric(8.0, 4.0))
                                .lock_focus(true)
                                .desired_width((ui.available_width() - 80.0).max(40.0));

                            let res = ui.add(text_edit);
                            let context_paste_image = crate::ui::input::input_context_menu(
                                &res,
                                &mut self.input_text,
                                selection_before_click,
                            );

                            let paste_shortcut = res.has_focus() && paste_shortcut_this_frame;
                            let paste_image = context_paste_image
                                || (res.has_focus() && paste_image_this_frame);
                            if paste_shortcut && !paste_image {
                                self.file_share_error = None;
                            } else if paste_image {
                                if let Some(buffer_id) = clipboard_upload_target(
                                    false,
                                    None,
                                    self.selected_buffer_id.as_deref(),
                                ) {
                                    self.start_matrix_clipboard_upload(buffer_id, ctx);
                                } else {
                                    self.file_share_error = Some(
                                        "Cannot paste: no chat is selected".to_owned(),
                                    );
                                }
                            }

                            if self.focus_input {
                                res.request_focus();
                                self.focus_input = false;
                            }

            if res.has_focus() {
                                if command_cancel {
                                    self.command_completion = None;
                                    self.command_completion_pending = None;
                                } else if command_up {
                                    self.move_command_completion_selection(-1);
                                    res.request_focus();
                                } else if command_down {
                                    self.move_command_completion_selection(1);
                                    res.request_focus();
                                } else if command_accept {
                                    self.accept_command_completion(None, ctx, res.id);
                                    res.request_focus();
                                } else if mention_cancel {
                                    self.mention_completion = None;
                                } else if mention_up {
                                    self.move_mention_selection(-1);
                                    res.request_focus();
                                } else if mention_down {
                                    self.move_mention_selection(1);
                                    res.request_focus();
                                } else if mention_accept {
                                    self.accept_mention(None, ctx, res.id);
                                    res.request_focus();
                                } else if tab_pressed {
                                    self.perform_completion(ctx, res.id);
                                    res.request_focus();
                                } else if history_up {
                                    self.cycle_history(-1, ctx, res.id);
                                    res.request_focus();
                                } else if history_down {
                                    self.cycle_history(1, ctx, res.id);
                                    res.request_focus();
                                } else {
                                    let any_other_key = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { pressed: true, .. })));
                                    if any_other_key && !tab_pressed && !history_up && !history_down {
                                        self.completion = None;
                                    }
                                }
                                if !mention_accept && !mention_cancel {
                                    self.refresh_mention_completion(ctx, res.id);
                                }
                                if res.changed() {
                                    self.reconcile_selected_mentions();
                                    self.request_command_completion(ctx, res.id);
                                }
                            }

                            if ui.add(egui::Button::new(egui::RichText::new("Send").color(Color32::WHITE).strong()).fill(accent_color).min_size(Vec2::new(60.0, 0.0))).clicked() || (res.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter))) {
                                self.send_current_message();
                                res.request_focus();
                            }

                            let popup_state = self.mention_completion.clone();
                            let mut clicked_mention = None;
                            if let Some(state) = popup_state {
                                let popup_id = res.id.with("mention_popup");
                                ui.memory_mut(|memory| memory.open_popup(popup_id));
                                egui::popup::popup_above_or_below_widget(
                                    ui,
                                    popup_id,
                                    &res,
                                    egui::AboveOrBelow::Above,
                                    |ui| {
                                        ui.set_min_width(res.rect.width().min(520.0));
                                        ui.set_max_width(res.rect.width().min(520.0));
                                        ui.label(
                                            egui::RichText::new("MENTION")
                                                .small()
                                                .strong()
                                                .color(accent_color),
                                        );
                                        egui::ScrollArea::vertical()
                                            .max_height(260.0)
                                            .show(ui, |ui| {
                                                for (index, candidate) in
                                                    state.matches.iter().enumerate()
                                                {
                                                    let selected = index == state.index;
                                                    let row_height =
                                                        (self.font_size + 10.0).max(28.0);
                                                    let (rect, response) = ui.allocate_exact_size(
                                                        egui::vec2(ui.available_width(), row_height),
                                                        egui::Sense::click(),
                                                    );
                                                    let visuals = ui
                                                        .style()
                                                        .interact_selectable(&response, selected);
                                                    ui.painter().rect(
                                                        rect,
                                                        visuals.rounding,
                                                        visuals.bg_fill,
                                                        visuals.bg_stroke,
                                                    );

                                                    let mut row_ui = ui.child_ui(
                                                        rect.shrink2(egui::vec2(6.0, 3.0)),
                                                        egui::Layout::left_to_right(
                                                            egui::Align::Center,
                                                        ),
                                                    );
                                                    row_ui.set_clip_rect(rect.shrink(3.0));
                                                    self.render_profile_identity(
                                                        &mut row_ui,
                                                        &candidate.display_name,
                                                        FontId::proportional(self.font_size),
                                                        visuals.text_color(),
                                                    );
                                                    if candidate.display_name
                                                        != candidate.user_id
                                                    {
                                                        row_ui.add_space(8.0);
                                                        self.render_profile_identity(
                                                            &mut row_ui,
                                                            &candidate.user_id,
                                                            FontId::proportional(
                                                                self.font_size * 0.88,
                                                            ),
                                                            text_muted,
                                                        );
                                                    }

                                                    if response.clicked() {
                                                        clicked_mention = Some(index);
                                                    }
                                                }
                                            });
                                    },
                                );
                            }
                            if let Some(index) = clicked_mention {
                                self.accept_mention(Some(index), ctx, res.id);
                                res.request_focus();
                            }

                            let popup_state = self.command_completion.clone();
                            let mut clicked_command = None;
                            if let Some(state) = popup_state {
                                let popup_id = res.id.with("command_popup");
                                ui.memory_mut(|memory| memory.open_popup(popup_id));
                                egui::popup::popup_above_or_below_widget(
                                    ui,
                                    popup_id,
                                    &res,
                                    egui::AboveOrBelow::Above,
                                    |ui| {
                                        ui.set_min_width(res.rect.width().min(560.0));
                                        ui.set_max_width(res.rect.width().min(560.0));
                                        let title = if state.context == "command" {
                                            format!("COMMANDS · {}", state.matches.len())
                                        } else {
                                            format!("ARGUMENTS · {}", state.matches.len())
                                        };
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                egui::RichText::new(title)
                                                    .small()
                                                    .strong()
                                                    .color(accent_color),
                                            );
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.label(
                                                        egui::RichText::new(
                                                            "↑↓ select · Tab/Enter insert · Esc close",
                                                        )
                                                        .small()
                                                        .color(text_muted),
                                                    );
                                                },
                                            );
                                        });
                                        egui::ScrollArea::vertical()
                                            .max_height(320.0)
                                            .show(ui, |ui| {
                                                for (index, candidate) in
                                                    state.matches.iter().enumerate()
                                                {
                                                    let label = if state.context == "command"
                                                        && !candidate.starts_with('/')
                                                    {
                                                        format!("/{candidate}")
                                                    } else {
                                                        candidate.clone()
                                                    };
                                                    if ui
                                                        .selectable_label(
                                                            index == state.index,
                                                            label,
                                                        )
                                                        .clicked()
                                                    {
                                                        clicked_command = Some(index);
                                                    }
                                                }
                                            });
                                    },
                                );
                            }
                            if let Some(index) = clicked_command {
                                self.accept_command_completion(Some(index), ctx, res.id);
                                res.request_focus();
                            }
                        });
                    });
                });
        }

        // Collect pending connect/disconnect actions from the connection panel

        egui::CentralPanel::default()
            .frame(Frame::none().fill(bg_color).inner_margin(Margin::same(0.0)))
            .show(ctx, |ui| {
            // egui panels use screen_rect as clip_rect — explicitly restrict to this
            // panel's rect so content cannot paint into adjacent side panels.
            ui.set_clip_rect(ui.max_rect().intersect(ui.clip_rect()));
            if self.show_settings {
                self.show_settings_window(ui, accent_color, is_light);
            } else if self.show_connections {
                self.show_connections_window(ui, accent_color, is_light);
            } else if self.show_connection_log {
                if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.show_connection_log = false;
                }
                ui.vertical_centered(|ui| {
                    ui.add_space(32.0);
                    let log_w = (ui.available_width() - 80.0).min(720.0);
                    ui.allocate_ui(egui::vec2(log_w, ui.available_height() - 32.0), |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Connection Log").strong().size(14.0));
                            // Connection selector tabs
                            for conn in &self.connections {
                                let selected = self.selected_conn_log.as_deref() == Some(&conn.prefix);
                                let dot_color = if conn.client.is_connected() {
                                    Color32::from_rgb(50, 205, 50)
                                } else if conn.connecting_pending {
                                    Color32::from_rgb(255, 165, 0)
                                } else {
                                    Color32::from_rgb(180, 60, 60)
                                };
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("●").color(dot_color).size(10.0));
                                    if ui.selectable_label(selected, &conn.label).clicked() {
                                        self.selected_conn_log = Some(conn.prefix.clone());
                                    }
                                });
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button(egui::RichText::new("✕").size(14.0)).on_hover_text("Close").clicked() {
                                    self.show_connection_log = false;
                                }
                            });
                        });
                        ui.add_space(8.0);
                        egui::Frame::none()
                            .fill(if is_light { Color32::from_gray(245) } else { Color32::from_rgb(14, 14, 14) })
                            .rounding(egui::Rounding::same(8.0))
                            .stroke(egui::Stroke::new(1.0, border_color))
                            .inner_margin(egui::Margin::same(12.0))
                            .show(ui, |ui| {
                                let log_font = egui::FontId::new(self.font_size * 0.88, egui::FontFamily::Monospace);
                                egui::ScrollArea::vertical()
                                    .stick_to_bottom(true)
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        ui.set_min_width(ui.available_width());
                                        // Find selected connection log
                                        let selected_prefix = self.selected_conn_log.clone();
                                        let log_entries: Vec<String> = self.connections.iter()
                                            .find(|c| selected_prefix.as_deref() == Some(&c.prefix))
                                            .map(|c| c.connection_log.iter().cloned().collect())
                                            .unwrap_or_default();
                                        let is_pending = self.connections.iter()
                                            .find(|c| selected_prefix.as_deref() == Some(&c.prefix))
                                            .map(|c| c.connecting_pending)
                                            .unwrap_or(false);
                                        if log_entries.is_empty() {
                                            ui.label(egui::RichText::new("No connection activity yet.").color(text_muted).italics());
                                        }
                                        for entry in &log_entries {
                                            let color = if entry.contains("Error") || entry.contains("failed") || entry.contains("Disconnected") {
                                                Color32::from_rgb(220, 80, 80)
                                            } else if entry.contains("Connected") {
                                                Color32::from_rgb(50, 205, 50)
                                            } else {
                                                text_secondary
                                            };
                                            ui.label(egui::RichText::new(entry).font(log_font.clone()).color(color));
                                        }
                                        if is_pending {
                                            ui.spinner();
                                        }
                                    });
                            });
                    });
                });
            } else if self.profiles.is_empty() && !any_connected {
                // No profiles yet — show a minimal landing page that opens the connections window
                ui.vertical_centered(|ui| {
                    ui.add_space(ctx.available_rect().height() * 0.2);
                    Frame::group(ui.style())
                        .fill(surface_color)
                        .rounding(Rounding::same(12.0))
                        .stroke(Stroke::new(1.0, border_color))
                        .inner_margin(Margin::same(40.0))
                        .show(ui, |ui| {
                            ui.set_max_width(360.0);
                            ui.heading(egui::RichText::new("No connections configured").strong().size(20.0));
                            ui.add_space(12.0);
                            ui.label(egui::RichText::new("Add a connection to get started.").color(text_secondary));
                            ui.add_space(20.0);
                            let btn = egui::Button::new(egui::RichText::new("+ Add Connection").strong().color(Color32::WHITE))
                                .fill(accent_color)
                                .min_size(Vec2::new(160.0, 40.0));
                            if ui.add(btn).clicked() {
                                self.show_connections = true;
                                self.conn_show_add = true;
                                self.editing_profile = ConnectionProfile::default();
                                self.editing_password.clear();
                                self.editing_profile_idx = None;
                            }
                        });
                });
            } else if let Some(_full_name) = current_buffer_full_name {
                ui.vertical(|ui| {
                    ui.set_max_width(ui.available_width());
                    if self.show_search {
                        Frame::none()
                            .fill(surface_color.linear_multiply(0.8))
                            .inner_margin(Margin::symmetric(16.0, 8.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label("🔍");
                                    let res = ui.add(egui::TextEdit::singleline(&mut self.search_text)
                                        .hint_text("Search scrollback...")
                                        .desired_width(ui.available_width() - 40.0));
                                    if ui.button("❌").clicked() {
                                        self.show_search = false;
                                        self.search_text.clear();
                                    }
                                    if self.show_search { res.request_focus(); }
                                });
                            });
                        ui.separator();
                    }

                    if current_buffer_is_replaced {
                        Frame::none()
                            .fill(Color32::from_rgb(120, 82, 24).linear_multiply(0.35))
                            .inner_margin(Margin::symmetric(16.0, 7.0))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(
                                        "Archived room history",
                                    )
                                    .color(Color32::from_rgb(255, 205, 115))
                                    .strong(),
                                );
                            });
                    }

                    if self.show_titlebar && (!current_buffer_topic.is_empty() || !current_buffer_modes.is_empty()) {
                        Frame::none()
                            .fill(surface_color.linear_multiply(0.3))
                            .inner_margin(Margin::symmetric(16.0, 6.0))
                            .stroke(Stroke::new(1.0, Color32::from_white_alpha(10)))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal_wrapped(|ui| {
                                    if !current_buffer_modes.is_empty() {
                                        ui.label(egui::RichText::new(format!("[{}]", current_buffer_modes)).color(accent_color).small());
                                    }
                                    if !current_buffer_topic.is_empty() {
                                        let topic_font = FontId::new(self.font_size, if self.use_monospace { FontFamily::Monospace } else { FontFamily::Proportional });
                                        let sections = ANSIParser::parse(&current_buffer_topic);
                                        let mut job = LayoutJob::default();
                                        for s in sections { job.append(&s.text, 0.0, s.style.to_format(topic_font.clone(), &render_theme)); }
                                        ui.add(Label::new(job).wrap(true));
                                    }
                                });
                            });
                        ui.add_space(-1.0);
                        ui.separator();
                    }

                    let chat_scroll_id = current_buffer_id
                        .as_deref()
                        .unwrap_or("no-buffer");
                    let mut history_view_at_top = false;
                    let mut history_view_away_from_top = false;
                    ScrollArea::vertical()
                        .id_source(("chat-scrollback", chat_scroll_id))
                        .stick_to_bottom(true)
                        .auto_shrink([false, false])
                        .horizontal_scroll_offset(0.0)
                        .show_viewport(ui, |ui, viewport| {
                        history_view_at_top = viewport.min.y <= 48.0;
                        history_view_away_from_top = viewport.min.y >= 96.0;
                        // Capture width inside the scroll area so it reflects inner_size.x
                        // (outer width minus vertical scrollbar), preventing content_is_too_large.x
                        // from going true and enabling horizontal offset drift via drag-to-scroll.
                        let msg_area_width = ui.available_width();
                        ui.set_min_width(msg_area_width);
                        ui.set_max_width(msg_area_width);
                        ui.spacing_mut().item_spacing.y = 1.0;
                        Frame::none().inner_margin(Margin::same(16.0)).show(ui, |ui| {
                            if let (Some(view_buffer_id), Some(load_buffer_id), Some(_messages)) = (
                                current_buffer_id.as_ref(),
                                current_history_load_buffer_id.as_ref(),
                                current_buffer_messages.as_ref(),
                            ) {
                                    ui.add_space(4.0);
                                    ui.horizontal(|ui| {
                                        ui.add_space((ui.available_width() - 180.0).max(0.0) / 2.0);
                                        if self.loading_more_buffer_id.as_deref() == Some(load_buffer_id.as_str()) {
                                            ui.spinner();
                                            ui.label(egui::RichText::new("Loading…").color(text_muted).small());
                                        } else if ui.button("⬆ Load older messages").clicked() {
                                            pending_load_more = Some((
                                                load_buffer_id.clone(),
                                                view_buffer_id.clone(),
                                            ));
                                        }
                                    });
                                    ui.add_space(8.0);
                                    ui.scope(|ui| {
                                        ui.visuals_mut().widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, border_color);
                                        ui.separator();
                                    });
                                    ui.add_space(4.0);
                            } else if current_history_exhausted {
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    ui.add_space((ui.available_width() - 150.0).max(0.0) / 2.0);
                                    ui.label(
                                        egui::RichText::new("No older messages")
                                            .color(text_muted)
                                            .small(),
                                    );
                                });
                                ui.add_space(8.0);
                                ui.scope(|ui| {
                                    ui.visuals_mut().widgets.noninteractive.bg_stroke =
                                        egui::Stroke::new(1.0, border_color);
                                    ui.separator();
                                });
                                ui.add_space(4.0);
                            }

                            if let Some(messages) = &current_buffer_messages {
                                let mut marker_shown = false;
                                let mut previous_visible_date: Option<String> = None;
                                let mut archived_years_shown = HashSet::new();
                                let visit_marker_state = current_buffer_visit_marker_id
                                    .as_deref()
                                    .map(|marker_id| visit_marker_location(messages, marker_id))
                                    .unwrap_or(VisitMarkerLocation::Missing);
                                let search_query = if self.search_text.is_empty() {
                                    None
                                } else {
                                    Some(self.search_text.to_lowercase())
                                };
                                let thread_button_line_ids: HashSet<&str> = thread_summaries
                                    .keys()
                                    .filter_map(|event_id| {
                                        messages
                                            .iter()
                                            .rev()
                                            .find(|line| {
                                                line.matrix_event_id.as_ref() == Some(event_id)
                                            })
                                            .map(|line| line.id.as_str())
                                    })
                                    .collect();
                                let reply_contexts = reply_contexts_by_event(messages);
                                let mut previous_matrix_event_id: Option<String> = None;
                                for (line_index, line) in messages.iter().enumerate() {
                                    if !self.show_filtered_lines && !line.displayed { continue; }
                                    if is_matrix_media_status_line(&line.plain_message) { continue; }
                                    let is_inherited_history =
                                        inherited_history_line_ids.contains(&line.id);

                                    if let Some((anchor_id, previous_len)) = self
                                        .history_scroll_anchors
                                        .get(current_buffer_id.as_deref().unwrap_or(""))
                                    {
                                        if line.id == *anchor_id && messages.len() > *previous_len {
                                            ui.scroll_to_cursor(Some(egui::Align::TOP));
                                            pending_clear_history_anchor = current_buffer_id.clone();
                                        }
                                    }

                                    if let Some(q) = &search_query {
                                        let reply_context_matches = line
                                            .matrix_event_id
                                            .as_ref()
                                            .and_then(|event_id| reply_contexts.get(event_id))
                                            .is_some_and(|reply| {
                                                reply
                                                    .sender
                                                    .as_deref()
                                                    .is_some_and(|sender| {
                                                        sender.to_lowercase().contains(q)
                                                    })
                                                    || reply.quotes.iter().any(|quote| {
                                                        quote.to_lowercase().contains(q)
                                                    })
                                            });
                                        if !line.plain_prefix_lower.contains(q)
                                            && !line.plain_message_lower.contains(q)
                                            && !reply_context_matches
                                        {
                                            continue;
                                        }
                                    }
                                    let local_timestamp =
                                        line.timestamp.with_timezone(&chrono::Local);
                                    let local_date = local_timestamp.format("%Y-%m-%d").to_string();
                                    let local_year = local_timestamp.format("%Y").to_string();
                                    if is_inherited_history
                                        && archived_years_shown.insert(local_year.clone())
                                    {
                                        ui.add_space(12.0);
                                        ui.horizontal(|ui| {
                                            ui.separator();
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    " ARCHIVED HISTORY · {local_year} "
                                                ))
                                                .color(accent_color)
                                                .strong()
                                                .size(11.0),
                                            );
                                            ui.separator();
                                        });
                                        ui.add_space(6.0);
                                    }
                                    if previous_visible_date.as_deref() != Some(&local_date) {
                                        ui.horizontal(|ui| {
                                            ui.separator();
                                            ui.label(
                                                egui::RichText::new(
                                                    local_timestamp.format(" %A, %e %B %Y ").to_string(),
                                                )
                                                .color(text_muted)
                                                .size(10.0),
                                            );
                                            ui.separator();
                                        });
                                        ui.add_space(4.0);
                                        previous_visible_date = Some(local_date);
                                    }
                                    let continues_matrix_event = line.matrix_event_id.is_some()
                                        && previous_matrix_event_id
                                            .as_ref()
                                            == line.matrix_event_id.as_ref();

                                    let past_visit_marker =
                                        visit_marker_state.is_before(line_index);
                                    if !marker_shown && past_visit_marker {
                                        let elapsed = self.selected_view_since
                                            .map(|t| t.elapsed())
                                            .unwrap_or(std::time::Duration::from_secs(99));
                                        let divider_color = Color32::from_rgb(200, 50, 50);
                                        ui.add_space(8.0);
                                        if elapsed < std::time::Duration::from_secs(2) {
                                            let remaining = std::time::Duration::from_secs(2) - elapsed;
                                            ctx.request_repaint_after(remaining);
                                            ui.horizontal(|ui| {
                                                ui.add_space(20.0);
                                                ui.separator();
                                                ui.label(egui::RichText::new(" NEW MESSAGES ").color(divider_color).size(10.0).strong());
                                                ui.separator();
                                            });
                                        } else {
                                            ui.scope(|ui| {
                                                ui.visuals_mut().widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, divider_color);
                                                ui.separator();
                                            });
                                        }
                                        ui.add_space(8.0);
                                        marker_shown = true;
                                    }

                                    // Reply quotes are rendered together with their header as one
                                    // compact context card. Keep an orphaned quote visible when a
                                    // history page starts after its header.
                                    if line.matrix_reply.as_ref().is_some_and(|reply| {
                                        matches!(reply.kind, MatrixReplyLineKind::Quote)
                                            && line
                                                .matrix_event_id
                                                .as_ref()
                                                .and_then(|event_id| {
                                                    reply_contexts.get(event_id)
                                                })
                                                .is_some_and(|context| context.has_header)
                                    }) {
                                        if !line.matrix_event_id.as_deref().is_some_and(
                                            |event_id| {
                                                matrix_event_has_visible_body_line(
                                                    messages, event_id,
                                                )
                                            },
                                        ) {
                                            previous_matrix_event_id =
                                                line.matrix_event_id.clone();
                                        }
                                        continue;
                                    }
                                    if redundant_room_reply_header(messages, line) {
                                        continue;
                                    }

                                    let is_own_mention = line.highlight
                                        || message_mentions_any_alias(
                                            &line.plain_message,
                                            &current_buffer_mention_aliases,
                                        );
                                    let mut row_hovered_url: Option<String> = None;
                                    let background_shape =
                                        ui.painter().add(egui::Shape::Noop);
                                    let row_resp = Frame::none()
                                        .rounding(Rounding::same(5.0))
                                        .inner_margin(Margin::symmetric(4.0, 2.0))
                                        .show(ui, |ui| {
                                    let row_width = ui.available_width();
                                    let compact_row = compact_message_row(row_width);
                                    ui.with_layout(
                                        egui::Layout::left_to_right(egui::Align::TOP)
                                            .with_main_wrap(compact_row),
                                        |ui| {
                                        ui.spacing_mut().item_spacing.x = 6.0;
                                        let mut avatar_rect: Option<Rect> = None;
                                        let mut prefix_visible_rect: Option<Rect> = None;
                                        if self.show_timestamps {
                                            ui.scope(|ui| {
                                                if continues_matrix_event {
                                                    ui.set_opacity(0.0);
                                                }
                                                ui.label(egui::RichText::new(line.timestamp.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()).font(font_id.clone()).color(text_muted));
                                            });
                                        }
                                        if current_buffer_is_matrix {
                                            if let (Some(buffer_id), Some(profiles)) = (
                                                current_buffer_id.as_deref(),
                                                current_buffer_member_profiles.as_deref(),
                                            ) {
                                                let avatar_response = ui.scope(|ui| {
                                                    if continues_matrix_event {
                                                        ui.set_opacity(0.0);
                                                    }
                                                    self.render_matrix_avatar(
                                                        ui,
                                                        buffer_id,
                                                        profiles,
                                                        &line.plain_prefix,
                                                        24.0,
                                                        accent_color,
                                                        Color32::WHITE,
                                                    )
                                                }).inner;
                                                if !continues_matrix_event {
                                                    avatar_rect = Some(avatar_response.rect);
                                                }
                                            }
                                        }
                                        let compact_prefix_sections = current_buffer_is_matrix
                                            .then(|| {
                                                compact_matrix_prefix_sections(
                                                    &line.plain_prefix,
                                                    &line.parsed_prefix,
                                                )
                                            });
                                        let prefix_sections = compact_prefix_sections
                                            .as_deref()
                                            .unwrap_or(&line.parsed_prefix);

                                        // Measure plain-text width for stable column tracking.
                                        let measured_text_w: f32 = prefix_sections
                                            .iter()
                                            .map(|section| {
                                                self.text_with_emoji_width(
                                                    ui,
                                                    &section.text,
                                                    &font_id,
                                                    true,
                                                )
                                            })
                                            .sum();
                                        // Keep a small right inset: the emoji-aware measuring
                                        // path and egui's individual span widgets can differ by
                                        // a couple of pixels at fractional scale factors.
                                        let measured_w = measured_text_w
                                            + if prefix_sections.is_empty() { 0.0 } else { 4.0 };
                                        let configured_cap_px = if self.prefix_align_max > 0 {
                                            ui.fonts(|f| {
                                                f.layout_no_wrap("M".repeat(self.prefix_align_max), font_id.clone(), Color32::WHITE).size().x
                                            })
                                        } else {
                                            f32::INFINITY
                                        };
                                        let cap_px = prefix_column_cap(
                                            configured_cap_px,
                                            row_width,
                                            self.show_timestamps,
                                        );
                                        let entry = self.prefix_col_widths.entry(current_buffer_id.clone().unwrap_or_default()).or_insert(0.0);
                                        let (next_col_width, grew) =
                                            update_prefix_column_width(*entry, measured_w, cap_px);
                                        *entry = next_col_width;
                                        if grew {
                                            // Earlier rows in this immediate-mode frame were
                                            // laid out with the old maximum. Repaint once so
                                            // every visible row uses the same final column.
                                            ui.ctx().request_repaint();
                                        }
                                        let col_width = *entry;

                                        let prefix_response = ui.allocate_ui_with_layout(
                                            egui::vec2(col_width, ui.text_style_height(&TextStyle::Body)),
                                            // ANSI color boundaries and emoji are separate
                                            // widgets. RTL layout reversed those widgets,
                                            // turning `&strk 🧭` into `strk 🧭&`.
                                            // Apply the left padding explicitly and keep the
                                            // logical prefix order left-to-right.
                                            prefix_span_layout(),
                                            |ui| {
                                                // A long sender must never paint through the
                                                // separator and over the message column.
                                                if measured_w > col_width {
                                                    ui.set_clip_rect(ui.clip_rect().intersect(ui.max_rect()));
                                                }
                                                if continues_matrix_event {
                                                    ui.set_opacity(0.0);
                                                }
                                                ui.spacing_mut().item_spacing.x = 0.0;
                                                ui.add_space((col_width - measured_w).max(0.0));
                                                for s in prefix_sections {
                                                    let format = s.style.to_format(font_id.clone(), &render_theme);
                                                    self.render_text_with_emoji(ui, &s.text, &format, false, true);
                                                }
                                            }
                                        ).response;
                                        if !continues_matrix_event {
                                            let visible_prefix_width = measured_w.min(col_width);
                                            prefix_visible_rect = Some(Rect::from_min_max(
                                                egui::pos2(
                                                    prefix_response.rect.max.x - visible_prefix_width,
                                                    prefix_response.rect.min.y,
                                                ),
                                                prefix_response.rect.max,
                                            ));
                                        }
                                        let author_card = avatar_rect.or(prefix_visible_rect).and_then(|_| {
                                            message_sender_profile_card(
                                                current_buffer_id.as_deref()?,
                                                &line.plain_prefix,
                                                current_buffer_server.as_deref().unwrap_or_default(),
                                                current_buffer_is_matrix,
                                                current_buffer_member_profiles
                                                    .as_deref()
                                                    .unwrap_or_default(),
                                                current_buffer_nicks
                                                    .as_deref()
                                                    .unwrap_or_default(),
                                            )
                                        });
                                        if let (Some(rect), Some(card)) =
                                            (avatar_rect, author_card.clone())
                                        {
                                            self.open_profile_on_author_click(
                                                ui,
                                                ui.make_persistent_id((
                                                    "message_author_profile",
                                                    "avatar",
                                                    &line.id,
                                                )),
                                                rect,
                                                Some(card),
                                            );
                                        }
                                        if let Some(rect) = prefix_visible_rect {
                                            self.open_profile_on_author_click(
                                                ui,
                                                ui.make_persistent_id((
                                                    "message_author_profile",
                                                    "name",
                                                    &line.id,
                                                )),
                                                rect,
                                                author_card,
                                            );
                                        }
                                        if !self.prefix_suffix.is_empty() {
                                            ui.scope(|ui| {
                                                if continues_matrix_event {
                                                    ui.set_opacity(0.0);
                                                }
                                                ui.label(egui::RichText::new(&self.prefix_suffix).font(font_id.clone()).color(text_muted));
                                            });
                                        }
                                        ui.spacing_mut().item_spacing.x = 0.0;
                                        ui.add_space(PREFIX_MESSAGE_GAP);

                                        let msg_col_width = if compact_row {
                                            row_width
                                        } else {
                                            ui.available_width()
                                        };
                                        ui.allocate_ui_with_layout(
                                            egui::vec2(msg_col_width, 0.0),
                                            egui::Layout::top_down(egui::Align::LEFT),
                                            |ui| {
                                            ui.set_min_width(msg_col_width);
                                            ui.set_max_width(msg_col_width);
                                            let message_previews = if let Some(reply) = &line.matrix_reply {
                                                let fallback_reply = ThreadReplyContext {
                                                    target_event_id: reply.event_id.clone(),
                                                    sender: reply.sender.clone(),
                                                    quotes: if matches!(
                                                        reply.kind,
                                                        MatrixReplyLineKind::Quote
                                                    ) {
                                                        vec![reply_quote_text(line)]
                                                    } else {
                                                        Vec::new()
                                                    },
                                                    has_header: matches!(
                                                        reply.kind,
                                                        MatrixReplyLineKind::Header
                                                    ),
                                                };
                                                let reply_context = line
                                                    .matrix_event_id
                                                    .as_ref()
                                                    .and_then(|event_id| {
                                                        reply_contexts.get(event_id)
                                                    })
                                                    .unwrap_or(&fallback_reply);
                                                let reply_response = render_reply_context_card(
                                                    ui,
                                                    reply_context,
                                                    card_bg,
                                                    accent_color,
                                                    text_secondary,
                                                );
                                                if let Some(event_id) =
                                                    reply.event_id.as_deref()
                                                {
                                                    reply_response
                                                        .on_hover_text(format!(
                                                            "Reply target: {event_id}"
                                                        ));
                                                }
                                                None
                                            } else {
                                                if !continues_matrix_event {
                                                    if let Some(reply_context) =
                                                        room_reply_context_for_body_line(
                                                            line,
                                                            &reply_contexts,
                                                        )
                                                    {
                                                        render_reply_context_card(
                                                            ui,
                                                            reply_context,
                                                            card_bg,
                                                            accent_color,
                                                            text_secondary,
                                                        );
                                                        ui.add_space(2.0);
                                                    }
                                                }
                                                let previews = self.render_message_content(
                                                    ui,
                                                    &line.message,
                                                    line.matrix_media.as_ref(),
                                                    current_buffer_id.as_deref(),
                                                    &font_id,
                                                    &render_theme,
                                                );
                                                row_hovered_url = previews.hovered_url.clone();
                                                Some(previews)
                                            };
                                            if let Some((thread_id, reply_count, unread)) =
                                                thread_button_line_ids
                                                    .contains(line.id.as_str())
                                                    .then(|| line.matrix_event_id.as_ref())
                                                    .flatten()
                                                    .and_then(|event_id| {
                                                        thread_summaries.get(event_id)
                                                    })
                                            {
                                                ui.add_space(3.0);
                                                let label = if *reply_count == 0 {
                                                    "💬 Open thread".to_owned()
                                                } else if *reply_count == 1 {
                                                    "💬 1 reply".to_owned()
                                                } else {
                                                    format!("💬 {} replies", reply_count)
                                                };
                                                let text = egui::RichText::new(label)
                                                    .strong()
                                                    .color(if *unread {
                                                        Color32::from_rgb(255, 120, 120)
                                                    } else {
                                                        accent_color
                                                    });
                                                let button = ui.small_button(text);
                                                if response_primary_clicked(ui, &button) {
                                                    pending_open_thread_buffer_id =
                                                        Some(thread_id.clone());
                                                }
                                            }

                                            if let Some(previews) = message_previews.as_ref() {
                                                self.render_message_previews(
                                                    ui,
                                                    previews,
                                                    text_secondary,
                                                    text_muted,
                                                    card_bg,
                                                    border_color,
                                                    accent_color,
                                                );
                                            }
                                        }); // end message column
                                    }); // end responsive message row
                                    }); // end highlight frame
                                    let plain_message = line.plain_message.clone();
                                    let plain_prefix = line.plain_prefix.clone();
                                    let interactable = row_resp.response.interact(egui::Sense::click());
                                    ui.painter().set(
                                        background_shape,
                                        egui::Shape::Vec(message_row_shapes(
                                            interactable.rect,
                                            is_own_mention,
                                            interactable.contains_pointer(),
                                            accent_color,
                                            text_primary,
                                        )),
                                    );
                                    // While the row is hovered (pointer in Middle layer, popup not
                                    // open), keep the stored URL current.  Once the popup opens the
                                    // pointer moves into the Foreground layer so the row is no longer
                                    // hovered, which freezes ctx_menu_hovered_url at the right value.
                                    if interactable.hovered() {
                                        self.ctx_menu_hovered_url = row_hovered_url.clone();
                                    }
                                    let menu_url = self.ctx_menu_hovered_url.clone();
                                    interactable.context_menu(|ui| {
                                        ui.set_min_width(170.0);
                                        if current_buffer_is_matrix {
                                            let event_id = line.matrix_event_id.clone();
                                            let thread_buffer_id = event_id
                                                .as_ref()
                                                .and_then(|event_id| {
                                                    thread_summaries.get(event_id)
                                                })
                                                .map(|(buffer_id, _, _)| buffer_id.clone());
                                            if ui
                                                .add_enabled(
                                                    event_id.is_some()
                                                        && !is_inherited_history
                                                        && !current_buffer_is_replaced,
                                                    egui::Button::new("Reply"),
                                                )
                                                .on_disabled_hover_text(
                                                    if is_inherited_history || current_buffer_is_replaced {
                                                        "This message belongs to a replaced Matrix room"
                                                    } else {
                                                        "This line has no Matrix event ID"
                                                    },
                                                )
                                                .clicked()
                                            {
                                                pending_reply_target = event_id.clone().map(
                                                    |matrix_event_id| ReplyTarget {
                                                        buffer_id: current_buffer_id
                                                            .clone()
                                                            .unwrap_or_default(),
                                                        matrix_event_id,
                                                        sender: plain_prefix.clone(),
                                                        message: plain_message.clone(),
                                                    },
                                                );
                                                ui.close_menu();
                                            }
                                            if let Some(thread_buffer_id) =
                                                thread_buffer_id.as_ref()
                                            {
                                                if ui.button("Open thread").clicked() {
                                                    pending_open_thread_buffer_id =
                                                        Some(thread_buffer_id.clone());
                                                    ui.close_menu();
                                                }
                                            }
                                            if ui
                                                .add_enabled(
                                                    event_id.is_some()
                                                        && !is_inherited_history
                                                        && !current_buffer_is_replaced,
                                                    egui::Button::new("Delete message…"),
                                                )
                                                .on_disabled_hover_text(
                                                    if is_inherited_history
                                                        || current_buffer_is_replaced
                                                    {
                                                        "This message belongs to a replaced Matrix room"
                                                    } else {
                                                        "This line has no Matrix event ID"
                                                    },
                                                )
                                                .clicked()
                                            {
                                                self.pending_redaction_error = None;
                                                let delete_preview = event_id
                                                    .as_ref()
                                                    .and_then(|matrix_event_id| {
                                                        current_buffer_messages.as_ref().and_then(
                                                            |messages| {
                                                                messages.iter().find_map(|message| {
                                                                    if message.matrix_event_id.as_ref()
                                                                        == Some(matrix_event_id)
                                                                    {
                                                                        message.matrix_media.as_ref()
                                                                    } else {
                                                                        None
                                                                    }
                                                                })
                                                            },
                                                        )
                                                    })
                                                    .map(|media| media.name.clone())
                                                    .unwrap_or_else(|| plain_message.clone());
                                                self.pending_redaction = event_id.map(
                                                    |matrix_event_id| RedactionTarget {
                                                        buffer_id: current_buffer_id
                                                            .clone()
                                                            .unwrap_or_default(),
                                                        matrix_event_id,
                                                        sender: plain_prefix.clone(),
                                                        message: delete_preview,
                                                    },
                                                );
                                                ui.close_menu();
                                            }
                                            ui.separator();
                                        }
                                        if let Some(ref url) = menu_url {
                                            if ui.button("Open URL").clicked() {
                                                ui.ctx()
                                                    .output_mut(|output| open_url_once(output, url));
                                                ui.close_menu();
                                            }
                                            if ui.button("Copy URL").clicked() {
                                                ui.ctx().output_mut(|o| o.copied_text = url.clone());
                                                ui.close_menu();
                                            }
                                            ui.separator();
                                        }
                                        if ui.button("Copy message").clicked() {
                                            ui.ctx().output_mut(|o| o.copied_text = plain_message.clone());
                                            ui.close_menu();
                                        }
                                        if !plain_prefix.is_empty() {
                                            if ui.button("Copy with sender").clicked() {
                                                ui.ctx().output_mut(|o| o.copied_text = format!("<{}> {}", plain_prefix, plain_message));
                                                ui.close_menu();
                                            }
                                        }
                                    });
                                    ui.add_space(1.0);
                                    previous_matrix_event_id =
                                        line.matrix_event_id.clone();
                                }
                            }
                            if self.force_scroll_to_bottom_buffer_id.as_deref()
                                == current_buffer_id.as_deref()
                            {
                                ui.add_space(0.0);
                                ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                                consumed_force_scroll_to_bottom = true;
                            }
                        });

                    if let Some(buf_id) = current_buffer_id.as_ref() {
                        if history_view_away_from_top {
                            pending_history_top_rearm = Some(buf_id.clone());
                        } else if history_view_at_top
                            && self.loading_more_buffer_id.is_none()
                            && current_history_load_buffer_id.is_some()
                            && current_buffer_messages.as_ref().is_some_and(|messages| {
                                should_auto_request_history(
                                    messages.len(),
                                    self.history_request_counts.contains_key(buf_id),
                                    self.history_top_armed_buffer_ids.contains(buf_id),
                                )
                            })
                        {
                            pending_load_more = current_history_load_buffer_id
                                .as_ref()
                                .map(|load_buffer_id| (load_buffer_id.clone(), buf_id.clone()));
                        }
                    }
                });
                });
            } else {
                ui.centered_and_justified(|ui| { ui.label(egui::RichText::new("Select a buffer to start chatting").color(text_muted).size(16.0)); });
            }
        });

        if let Some(buf_id) = pending_history_top_rearm {
            self.history_top_armed_buffer_ids.insert(buf_id);
        }
        if let Some(buf_id) = pending_clear_history_anchor {
            self.history_scroll_anchors.remove(&buf_id);
        }
        if consumed_force_scroll_to_bottom {
            self.force_scroll_to_bottom_buffer_id = None;
        }
        if let Some((load_buffer_id, view_buffer_id)) = pending_load_more {
            self.request_older_history(&load_buffer_id, &view_buffer_id);
        }

        let mut confirm_redaction = false;
        let mut cancel_redaction = false;
        if let Some(target) = self.pending_redaction.clone() {
            egui::Window::new("Delete Matrix message?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(
                        "This sends a Matrix redaction. The server may reject it if you do not have permission.",
                    );
                    ui.add_space(6.0);
                    let sender: String = ANSIParser::parse(&target.sender)
                        .iter()
                        .map(|section| section.text.as_str())
                        .collect();
                    let preview: String = target.message.chars().take(240).collect();
                    Frame::none()
                        .fill(surface_color)
                        .rounding(Rounding::same(6.0))
                        .inner_margin(Margin::same(8.0))
                        .show(ui, |ui| {
                            if !sender.is_empty() {
                                ui.label(
                                    egui::RichText::new(sender)
                                        .strong()
                                        .color(accent_color),
                                );
                            }
                            ui.label(preview);
                        });
                    if let Some(error) = self.pending_redaction_error.as_deref() {
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(format!("⚠ {error}"))
                                .color(Color32::from_rgb(220, 80, 80)),
                        );
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            cancel_redaction = true;
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Delete message")
                                        .color(Color32::WHITE)
                                        .strong(),
                                )
                                .fill(Color32::from_rgb(180, 45, 45)),
                            )
                            .clicked()
                        {
                            confirm_redaction = true;
                        }
                    });
                });
            if cancel_redaction {
                self.pending_redaction = None;
                self.pending_redaction_error = None;
            } else if confirm_redaction {
                match self.send_matrix_redaction(
                    &target.buffer_id,
                    &target.matrix_event_id,
                ) {
                    Ok(()) => {
                        self.pending_redaction = None;
                        self.pending_redaction_error = None;
                    }
                    Err(error) => self.pending_redaction_error = Some(error),
                }
            }
        }

        if let Some(reply_target) = pending_reply_target {
            self.reply_target = Some(reply_target);
            self.focus_input = true;
        }
        if let Some(thread_buffer_id) = pending_open_thread_buffer_id {
            self.open_thread(thread_buffer_id);
        }

        if any_connected {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
#[path = "render_harness.rs"]
mod render_harness;
