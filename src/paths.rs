use std::{
    fs, io,
    path::{Path, PathBuf},
};

use axum::http::StatusCode;

use crate::ApiError;

pub const UPLOAD_PREFIX: &str = ".zfs-upload-";

pub fn components(path: &str) -> Result<Vec<&str>, ApiError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    path.split('/')
        .map(|part| {
            let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
            let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
                || ["COM", "LPT"].iter().any(|prefix| {
                    stem.strip_prefix(prefix).is_some_and(|suffix| {
                        matches!(
                            suffix,
                            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                        )
                    })
                });
            if part.is_empty()
                || matches!(part, "." | "..")
                || device
                || part.ends_with(['.', ' '])
                || part.to_ascii_lowercase().starts_with(UPLOAD_PREFIX)
                || part
                    .chars()
                    .any(|c| c.is_control() || "\\:<>\"|?*".contains(c))
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "Invalid relative path or reserved filename",
                ));
            }
            Ok(part)
        })
        .collect()
}

pub fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub fn resolve(root: &Path, relative: &str) -> Result<PathBuf, ApiError> {
    let mut result = root.to_path_buf();
    for part in components(relative)? {
        result.push(part);
        let metadata = fs::symlink_metadata(&result)?;
        if is_link(&metadata) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "Symbolic links and Windows reparse points are not served",
            ));
        }
    }
    Ok(result)
}

pub fn destination(root: &Path, relative: &str) -> Result<PathBuf, ApiError> {
    let parts = components(relative)?;
    let name = parts
        .last()
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "A filename is required"))?;
    let parent = resolve(root, &parts[..parts.len() - 1].join("/"))?;
    if !parent.is_dir() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Parent is not a directory",
        ));
    }
    let result = parent.join(name);
    match fs::symlink_metadata(&result) {
        Ok(_) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "The destination already exists; overwriting is disabled",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(result),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_windows_escape_and_ambiguous_paths() {
        for path in [
            "../secret",
            "/absolute",
            "a//b",
            "a/",
            "C:/x",
            "\\\\host\\x",
            "file:stream",
            "a\\..\\b",
            "CON.txt",
            "a/LPT1",
            "COM¹",
            "trailing.",
            "trailing ",
            ".",
            "a\0b",
            ".ZFS-UPLOAD-hidden",
            "file?",
        ] {
            assert!(components(path).is_err(), "{path}");
        }
        assert!(components("").unwrap().is_empty());
        assert!(components("normal directory/nested/file.txt").is_ok());
        assert!(components("CONtent/COM10.txt").is_ok());
    }
}
