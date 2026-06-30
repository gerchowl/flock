use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STAGED_CLIPBOARD_IMAGE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) struct StagedClipboardImage {
    pub(crate) path: PathBuf,
    pub(crate) paste_text: String,
}

pub(crate) fn stage(
    client_id: u64,
    extension: &str,
    data: &[u8],
) -> io::Result<StagedClipboardImage> {
    use std::os::unix::fs::OpenOptionsExt;

    let extension = sanitize_extension(extension);
    let dir = ensure_staging_dir()?;
    cleanup_stale(&dir);

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    for attempt in 0..100 {
        let path = dir.join(format!(
            "client-{client_id}-clipboard-{unique}-{attempt}.{extension}"
        ));
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };
        file.write_all(data)?;
        let path_str = path.to_string_lossy().into_owned();
        let paste_text = if is_image_extension(&extension) {
            // A bare image path is what Claude Code captures as `[Image #N]`.
            path_str
        } else {
            // A bare path is opaque for a non-image file (and for non-CC
            // agents), so inject an explicit, agent-agnostic instruction (#79)
            // pointing at the SERVER-side staged copy.
            format!("read this file: {path_str}")
        };
        return Ok(StagedClipboardImage { paste_text, path });
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate unique clipboard image staging path",
    ))
}

pub(crate) fn remove_files(paths: Vec<PathBuf>) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

/// Reduce a client-supplied extension to a safe, real extension to stage under
/// (#79). Takes only the bare token after the last `.`/`/`/`\`, lowercased,
/// and keeps it only when it's a short alphanumeric string — so a PDF stays
/// `.pdf` while traversal/garbage (`../etc`, spaces, overlong) becomes an
/// opaque `.bin`. Generalised from the old image-only allowlist that forced
/// everything to `.png`.
fn sanitize_extension(extension: &str) -> String {
    let ext = extension
        .rsplit(['/', '.', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !ext.is_empty() && ext.len() <= 8 && ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
        ext
    } else {
        "bin".to_string()
    }
}

fn is_image_extension(extension: &str) -> bool {
    matches!(extension, "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp")
}

fn staging_dir() -> PathBuf {
    let user_id = unsafe { libc::geteuid() };
    std::env::temp_dir().join(format!("flock-clipboard-images-{user_id}"))
}

fn ensure_staging_dir() -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let dir = staging_dir();
    fs::create_dir_all(&dir)?;
    let metadata = fs::metadata(&dir)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "clipboard image staging path is not a directory: {}",
            dir.display()
        )));
    }
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

fn cleanup_stale(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified.elapsed().unwrap_or_default() > STAGED_CLIPBOARD_IMAGE_MAX_AGE {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_extension_keeps_safe_real_extensions() {
        assert_eq!(sanitize_extension("PNG"), "png");
        assert_eq!(sanitize_extension("jpeg"), "jpeg");
        assert_eq!(sanitize_extension("webp"), "webp");
        // Non-image extensions are now preserved (no longer forced to png).
        assert_eq!(sanitize_extension("pdf"), "pdf");
        assert_eq!(sanitize_extension("md"), "md");
        // A full filename reduces to its bare token; traversal is stripped.
        assert_eq!(sanitize_extension("report.pdf"), "pdf");
        assert_eq!(sanitize_extension("../../etc/passwd"), "passwd");
        // Garbage / unsafe / overlong falls back to an opaque blob.
        assert_eq!(sanitize_extension(""), "bin");
        assert_eq!(sanitize_extension("a b"), "bin");
        assert_eq!(sanitize_extension("toolongextension"), "bin");
    }

    #[test]
    fn stage_image_injects_bare_path_doc_injects_read_instruction() {
        // Image: bare path (so Claude Code captures it as [Image #N]).
        let img = stage(7, "png", b"\x89PNG\r\n").unwrap();
        assert!(img.path.to_string_lossy().ends_with(".png"));
        assert_eq!(img.paste_text, img.path.to_string_lossy());
        let _ = fs::remove_file(&img.path);

        // Non-image: real extension kept + explicit, agent-agnostic instruction.
        let doc = stage(7, "pdf", b"%PDF-1.7 hello").unwrap();
        let doc_path = doc.path.to_string_lossy().into_owned();
        assert!(
            doc_path.ends_with(".pdf"),
            "kept real extension: {doc_path}"
        );
        assert!(
            doc.paste_text.contains("read this file"),
            "{}",
            doc.paste_text
        );
        assert!(doc.paste_text.contains(&doc_path), "{}", doc.paste_text);
        assert_eq!(fs::read(&doc.path).unwrap(), b"%PDF-1.7 hello");
        let _ = fs::remove_file(&doc.path);
    }
}
