//! Injectable installation evidence for the client listener forwarding guard.
use std::{fs, io, path::PathBuf};

/// Candidate CLI and LocalAPI locations; tests supply only private fixture paths.
pub(crate) struct Locations {
    /// Fully expanded PATH candidates, in lookup order.
    pub path: Vec<PathBuf>,
    /// Platform app-bundle CLI candidates, after PATH.
    pub bundles: Vec<PathBuf>,
    /// LocalAPI paths whose presence disproves absence even without a CLI.
    pub sockets: Vec<PathBuf>,
}
impl Locations {
    /// Capture production candidates without running any external command.
    pub fn current() -> Self {
        // DefaultTailscaledSocket documents macOS's distinct socket name:
        // https://github.com/tailscale/tailscale/blob/main/paths/paths.go
        let (bundles, sockets) = if cfg!(target_os = "macos") {
            (
                vec!["/Applications/Tailscale.app/Contents/MacOS/Tailscale".into()],
                vec!["/var/run/tailscaled.socket".into()],
            )
        } else {
            (
                Vec::new(),
                vec![
                    "/var/run/tailscale/tailscaled.sock".into(),
                    "/run/tailscale/tailscaled.sock".into(),
                ],
            )
        };
        Self {
            path: std::env::var_os("PATH")
                .map(|path| {
                    std::env::split_paths(&path)
                        .map(|p| p.join("tailscale"))
                        .collect()
                })
                .unwrap_or_default(),
            bundles,
            sockets,
        }
    }
    /// Every candidate must be observably absent before the guard can be skipped.
    pub fn discover(&self) -> io::Result<Option<PathBuf>> {
        for path in self.path.iter().chain(&self.bundles) {
            match fs::symlink_metadata(path) {
                // Return even an unusable CLI: its execution must fail closed.
                Ok(_) => return Ok(Some(path.clone())),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "cannot inspect Tailscale CLI {}: {error}",
                        path.display()
                    )))
                }
            }
        }
        for path in &self.sockets {
            match fs::symlink_metadata(path) {
                Ok(_) => {
                    return Err(io::Error::other(format!(
                        "Tailscale socket present at {} but no CLI to read serve status",
                        path.display()
                    )))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "cannot inspect Tailscale socket {}: {error}",
                        path.display()
                    )))
                }
            }
        }
        Ok(None)
    }
    /// Describe every candidate inspected to establish positive absence.
    pub fn checked_paths(&self) -> String {
        self.path
            .iter()
            .chain(&self.bundles)
            .chain(&self.sockets)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}
