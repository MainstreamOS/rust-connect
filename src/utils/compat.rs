//! Filesystem and object identifiers at the desktop compatibility boundary.
use crate::utils::errors::Result;
use std::path::Path;

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
