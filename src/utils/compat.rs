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
