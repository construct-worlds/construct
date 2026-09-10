//! Turn explicit image paths in a user prompt into stable, provider-agnostic
//! image inputs.
//!
//! Construct's clipboard and drag/drop paths write binary payloads into the
//! session attachment directory, then paste the daemon-host path into the
//! harness. Smith also accepts paths supplied directly by users (including
//! relative paths and the `[#file:…]` attachment token used by web clients).
//! We snapshot bytes at submit time so replay after a daemon restart sends the
//! same image even if the original project file later changes.

use crate::provider::{Content, ImageInput};
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Anthropic's per-image limit is the tightest of Smith's native providers.
/// Applying it at the shared boundary keeps a persisted turn portable across
/// later `/model` switches rather than accepting a payload another provider
/// cannot replay.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_IMAGES_PER_TURN: usize = 20;

/// Preserve text-only turns byte-for-byte. A multimodal turn retains the same
/// text and adds snapshots for every explicit local image reference found.
pub fn user_content(text: String, cwd: &Path) -> Result<Content> {
    let candidates = image_path_candidates(&text, cwd);
    if candidates.is_empty() {
        return Ok(Content::Text { text });
    }
    if candidates.len() > MAX_IMAGES_PER_TURN {
        bail!(
            "image input contains {} images (maximum {})",
            candidates.len(),
            MAX_IMAGES_PER_TURN
        );
    }

    let images = candidates
        .into_iter()
        .map(|path| load_image(&path))
        .collect::<Result<Vec<_>>>()?;
    Ok(Content::UserInput { text, images })
}

fn load_image(path: &Path) -> Result<ImageInput> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("image input not found: {}", path.display()))?;
    if !metadata.is_file() {
        bail!("image input is not a file: {}", path.display());
    }
    if metadata.len() == 0 {
        bail!("image input is empty: {}", path.display());
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        bail!(
            "image input is too large: {} is {} bytes (maximum {} bytes)",
            path.display(),
            metadata.len(),
            MAX_IMAGE_BYTES
        );
    }

    let bytes =
        std::fs::read(path).with_context(|| format!("read image input {}", path.display()))?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        bail!(
            "image input is too large: {} is {} bytes (maximum {} bytes)",
            path.display(),
            bytes.len(),
            MAX_IMAGE_BYTES
        );
    }
    let format = image::guess_format(&bytes)
        .with_context(|| format!("unsupported or invalid image input: {}", path.display()))?;
    let media_type = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        other => bail!(
            "unsupported image format {:?} for {}; use PNG, JPEG, GIF, or WebP",
            other,
            path.display()
        ),
    };

    Ok(ImageInput {
        media_type: media_type.to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        source: Some(path.display().to_string()),
    })
}

fn image_path_candidates(text: &str, cwd: &Path) -> Vec<PathBuf> {
    let mut raw = Vec::new();
    collect_file_references(text, &mut raw);
    collect_markdown_images(text, &mut raw);

    let trimmed = text.trim();
    if looks_like_standalone_path(trimmed) {
        raw.push(trimmed.to_string());
    } else {
        for word in terminal_tokens(text) {
            let token = word.trim_matches(|c: char| {
                matches!(
                    c,
                    '"' | '\'' | '`' | ',' | ';' | ':' | '(' | ')' | '[' | ']'
                )
            });
            if looks_like_path(token) {
                raw.push(token.to_string());
            }
        }
    }

    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for candidate in raw {
        let Some(path) = normalize_candidate(&candidate, cwd) else {
            continue;
        };
        if !has_image_extension(&path) {
            continue;
        }
        let key = path.to_string_lossy().into_owned();
        if seen.insert(key) {
            paths.push(path);
        }
    }
    paths
}

fn collect_file_references(text: &str, out: &mut Vec<String>) {
    let mut rest = text;
    while let Some(start) = rest.find("[#file:") {
        let value = &rest[start + "[#file:".len()..];
        let Some(end) = value.find(']') else {
            break;
        };
        out.push(value[..end].to_string());
        rest = &value[end + 1..];
    }
}

fn collect_markdown_images(text: &str, out: &mut Vec<String>) {
    let mut rest = text;
    while let Some(start) = rest.find("![") {
        let after_alt = &rest[start + 2..];
        let Some(close_alt) = after_alt.find("](") else {
            break;
        };
        let target = &after_alt[close_alt + 2..];
        let Some(close_target) = target.find(')') else {
            break;
        };
        out.push(target[..close_target].to_string());
        rest = &target[close_target + 1..];
    }
}

fn looks_like_path(raw: &str) -> bool {
    let token = raw
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '<' | '>'));
    if token.is_empty() || token.contains('\n') {
        return false;
    }
    if token.contains("://") && !token.starts_with("file://") {
        return false;
    }
    let pathish = token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.starts_with("file://");
    pathish && has_image_extension(Path::new(token))
}

fn looks_like_standalone_path(raw: &str) -> bool {
    let quoted = (raw.starts_with('"') && raw.ends_with('"'))
        || (raw.starts_with('\'') && raw.ends_with('\''));
    let token = raw
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '<' | '>'));
    if token.contains("://") && !token.starts_with("file://") {
        return false;
    }
    looks_like_path(token)
        || (quoted && has_image_extension(Path::new(token)))
        || (!token.chars().any(char::is_whitespace) && has_image_extension(Path::new(token)))
        || (token.contains("\\ ") && has_image_extension(Path::new(token)))
}

/// Split ordinary prose while retaining terminal-escaped spaces in a pasted
/// path. Quotes are retained for the normal trimming pass above.
fn terminal_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    let mut quote = None;
    while let Some(c) = chars.next() {
        if c == '\\' {
            current.push(c);
            if let Some(next) = chars.next() {
                current.push(next);
            }
        } else if matches!(c, '"' | '\'') {
            current.push(c);
            if quote == Some(c) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(c);
            }
        } else if c.is_whitespace() && quote.is_none() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn normalize_candidate(raw: &str, cwd: &Path) -> Option<PathBuf> {
    let mut token = raw
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '<' | '>'))
        .to_string();
    if token.contains("://") && !token.starts_with("file://") {
        return None;
    }
    if let Some(path) = token.strip_prefix("file://") {
        token = path.to_string();
    }
    token = unescape_terminal_path(&token)?;

    if let Some(rest) = token.strip_prefix("~/") {
        let home = std::env::var_os("HOME")?;
        return Some(PathBuf::from(home).join(rest));
    }
    let path = PathBuf::from(token);
    Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

fn unescape_terminal_path(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(chars.next()?);
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .is_some_and(|ext| {
            matches!(
                ext.as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn write_png(path: &Path) {
        let image = image::RgbaImage::from_pixel(2, 1, image::Rgba([1, 2, 3, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        std::fs::write(path, bytes.into_inner()).unwrap();
    }

    #[test]
    fn text_without_an_explicit_image_stays_plain_text() {
        let content =
            user_content("please edit image.png later".into(), Path::new("/tmp")).unwrap();
        assert!(matches!(content, Content::Text { text } if text == "please edit image.png later"));

        let content =
            user_content("please edit the screenshot later".into(), Path::new("/tmp")).unwrap();
        assert!(
            matches!(content, Content::Text { text } if text == "please edit the screenshot later")
        );

        let remote = "![remote](https://example.com/image.png)";
        let content = user_content(remote.into(), Path::new("/tmp")).unwrap();
        assert!(matches!(content, Content::Text { text } if text == remote));
    }

    #[test]
    fn missing_standalone_image_is_reported() {
        let error = user_content("missing.png".into(), Path::new("/tmp")).unwrap_err();
        assert!(error.to_string().contains("image input not found"));
    }

    #[test]
    fn loads_bare_attachment_and_file_reference_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        write_png(&path);

        for prompt in [
            path.display().to_string(),
            format!("inspect [#file:{}]", path.display()),
            "inspect ./shot.png".to_string(),
            "inspect ![shot](./shot.png)".to_string(),
        ] {
            let content = user_content(prompt.clone(), dir.path()).unwrap();
            let Content::UserInput { text, images } = content else {
                panic!("expected image input for {prompt}");
            };
            assert_eq!(text, prompt);
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].media_type, "image/png");
            assert!(!images[0].data.is_empty());
        }
    }

    #[test]
    fn loads_terminal_escaped_space_and_deduplicates_references() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("screen shot.png");
        write_png(&path);
        let escaped = path.display().to_string().replace(' ', "\\ ");
        let prompt = format!("{escaped} [#file:{}]", path.display());
        let content = user_content(prompt, dir.path()).unwrap();
        let Content::UserInput { images, .. } = content else {
            panic!("expected image input");
        };
        assert_eq!(images.len(), 1);

        let quoted = format!("inspect \"{}\"", path.display());
        let content = user_content(quoted, dir.path()).unwrap();
        assert!(matches!(content, Content::UserInput { images, .. } if images.len() == 1));
    }
}
