//! Build-script support for a prebuilt, symlink-free GUI dist.

use std::{fs, io, path::Path};

/// Generate the embed declaration without rewriting unchanged content.
///
/// Return whether the dist exists and can be watched by Cargo. A missing dist
/// still generates an empty embed for the router's typed HTTP refusal.
/// Reject symlinks at the root or anywhere inside the dist, including dangling
/// links. Inputs must remain unchanged during compilation (Nix store outputs
/// provide that guarantee). No dependency fetching or frontend build runs here.
pub fn prepare(path: &Path, output: &Path) -> io::Result<bool> {
    // Trailing separators or '/.' make lstat follow a directory symlink.
    // Normalize components lexically without resolving any symlink target.
    let normalized = path.components().collect::<std::path::PathBuf>();
    let path = normalized.as_path();
    let exists = match fs::symlink_metadata(path) {
        Ok(_) => {
            reject_symlinks(path)?;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    let folder = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "FLOCK_GUI_DIST must be valid UTF-8",
        )
    })?;
    let code = format!(
        "#[derive(rust_embed::RustEmbed)]\n#[folder = {folder:?}]\n#[allow_missing = true]\nstruct GuiAssets;\n"
    );
    let generated = output.join("gui_assets.rs");
    let changed = match fs::read(&generated) {
        Ok(previous) => previous != code.as_bytes(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => return Err(error),
    };
    if changed {
        fs::write(generated, code)?;
    }
    Ok(exists)
}

fn reject_symlinks(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("GUI dist refuses symlink: {}", path.display()),
        ));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            reject_symlinks(&entry?.path())?;
        }
    }
    Ok(())
}
