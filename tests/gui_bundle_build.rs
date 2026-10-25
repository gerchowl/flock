//! Exercise the actual build-script bundle generator without a frontend build.

#[path = "../src/web/bundle_build.rs"]
mod bundle_build;

use std::{fs, io, path::Path, time::SystemTime};

#[test]
fn missing_dist_is_not_watched_and_unchanged_declarations_are_not_rewritten() {
    let root = OwnedDir::new("missing");
    let output = root.path().join("out");
    fs::create_dir(&output).unwrap();
    let dist = root.path().join("missing");
    assert!(!bundle_build::prepare(&dist, &output).unwrap());
    let generated = output.join("gui_assets.rs");
    let content = fs::read(&generated).unwrap();
    let pinned = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
    fs::File::options()
        .write(true)
        .open(&generated)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(pinned))
        .unwrap();
    assert!(!bundle_build::prepare(&dist, &output).unwrap());
    assert_eq!(fs::read(&generated).unwrap(), content);
    assert_eq!(
        fs::metadata(&generated).unwrap().modified().unwrap(),
        pinned
    );
    fs::create_dir(&dist).unwrap();
    fs::write(dist.join("index.html"), "owned fixture").unwrap();
    assert!(bundle_build::prepare(&dist, &output).unwrap());
    assert_eq!(
        fs::metadata(&generated).unwrap().modified().unwrap(),
        pinned
    );
    assert!(bundle_build::prepare(&dist.join("."), &output).unwrap());
    assert_eq!(
        fs::metadata(&generated).unwrap().modified().unwrap(),
        pinned
    );
    let other = root.path().join("other");
    fs::create_dir(&other).unwrap();
    assert!(bundle_build::prepare(&other, &output).unwrap());
    assert_ne!(fs::read(&generated).unwrap(), content);
}

#[test]
fn dist_root_files_directories_and_dangling_symlinks_are_refused() {
    let root = OwnedDir::new("symlinks");
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "outside the chosen dist").unwrap();
    for case in [
        "root",
        "root-slash",
        "root-dot",
        "file",
        "directory",
        "dangling",
    ] {
        let base = root.path().join(case);
        fs::create_dir(&base).unwrap();
        let dist = base.join("dist");
        let output = base.join("out");
        fs::create_dir(&output).unwrap();
        if case.starts_with("root") {
            symlink(&outside, &dist);
        } else {
            fs::create_dir(&dist).unwrap();
            let target = match case {
                "file" => outside.join("secret.txt"),
                "directory" => outside.clone(),
                _ => outside.join("missing"),
            };
            symlink(&target, &dist.join("escape"));
        }
        let mut spelling = dist.clone().into_os_string();
        match case {
            "root-slash" => spelling.push("/"),
            "root-dot" => spelling.push("/."),
            _ => {}
        }
        let error = bundle_build::prepare(Path::new(&spelling), &output).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{case}");
        assert!(error.to_string().contains("GUI dist refuses symlink"));
        assert!(!output.join("gui_assets.rs").exists());
    }
}

fn symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

struct OwnedDir(std::path::PathBuf);
impl OwnedDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("gui-build-{label}-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for OwnedDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
