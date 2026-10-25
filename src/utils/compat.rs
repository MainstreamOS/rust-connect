//! Filesystem and object identifiers at the desktop compatibility boundary.
use crate::utils::errors::Result;
use std::path::Path;

/// Stable device-scoped identity for desktop notification history and replay.
pub fn notification_key(device: &str, id: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(format!("{device}\0{id}").as_bytes())[..12])
}

/// A local URL must stay a URL: bare paths are interpreted as theme icons by
/// some notification servers. Encode delimiters in user-controlled directories.
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(byte) {
            uri.push(*byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri
}

/// File extensions and the media types they stand for. The first extension
/// listed for a type is the one a file of that type is given.
const MEDIA_TYPES: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("jpg", "image/jpg"),
    ("png", "image/png"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("heic", "image/heic"),
    ("avif", "image/avif"),
    ("mp4", "video/mp4"),
    ("m4v", "video/mp4"),
    ("3gp", "video/3gpp"),
    ("mov", "video/quicktime"),
    ("mkv", "video/x-matroska"),
    ("webm", "video/webm"),
    ("m4a", "audio/mp4"),
    ("aac", "audio/mp4"),
    ("aac", "audio/aac"),
    ("3gp", "audio/3gpp"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("amr", "audio/amr"),
    ("ogg", "audio/ogg"),
    ("opus", "audio/ogg"),
    ("pdf", "application/pdf"),
    ("txt", "text/plain"),
    ("vcf", "text/x-vcard"),
    ("vcf", "text/vcard"),
];
/// The media type a file extension stands for.
pub fn media_type(extension: &str) -> Option<&'static str> {
    let extension = extension.to_ascii_lowercase();
    MEDIA_TYPES
        .iter()
        .find(|(ext, _)| *ext == extension)
        .map(|(_, mime)| *mime)
}
/// The extension a file of `mime` is given, ignoring parameters such as a charset.
pub fn media_extension(mime: &str) -> Option<&'static str> {
    let mime = mime
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    MEDIA_TYPES
        .iter()
        .find(|(_, kind)| *kind == mime)
        .map(|(ext, _)| *ext)
}

/// Preserve normal KDE IDs; escape the reserved prefix as well to avoid collisions.
pub fn component(value: &str) -> String {
    if !value.is_empty()
        && !value.starts_with("rc_")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        value.into()
    } else {
        format!("rc_{}", hex::encode(value.as_bytes()))
    }
}
pub async fn private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::create_dir_all(path).await?;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    Ok(())
}
pub async fn private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}
