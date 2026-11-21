//! Bringing the prepared game files onto a device with no launcher (iOS): `horizon-resources.zip`
//! made by `horizon-prepare` (or the "Prepare game files" workflow) is unpacked into the
//! `resources` folder, from a zip put in the app's folder with the Files app, or downloaded from a
//! link the player copied. Horizon ships none of Mojang's files, and neither does this: it only
//! moves the player's own prepared copy into place.
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// The name the prepare workflow gives its release file.
pub const ARCHIVE_NAME: &str = "horizon-resources.zip";
/// The file that makes a folder a set of prepared game files.
const MARKER: &str = "horizon-resources.json";
/// Unpacking stops past this many bytes: a zip that claims more is not the game files.
const MAX_UNPACKED: u64 = 24 << 30;
/// The most a link may download.
const MAX_DOWNLOAD: u64 = 16 << 30;

/// What an import is doing, for the screen.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    /// Bytes received and the total when the server said it.
    Downloading(u64, Option<u64>),
    /// Files unpacked of all.
    Unpacking(u64, u64),
    Done,
    Failed(String),
}

/// An import running on its own thread; the screen polls [`Import::status`].
#[derive(Clone)]
pub struct Import {
    status: Arc<Mutex<Status>>,
    cancel: Arc<AtomicBool>,
}

impl Import {
    /// Download `url` into `folder`, then unpack it into `resources`. When `url` is `None`, an
    /// archive already in `folder` is unpacked.
    pub fn start(url: Option<String>, folder: PathBuf, resources: PathBuf) -> Self {
        let import = Self {
            status: Arc::new(Mutex::new(Status::Idle)),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let this = import.clone();
        let spawned = std::thread::Builder::new()
            .name("Resource import".into())
            .spawn(move || {
                let result = this.run(url, &folder, &resources);
                this.set(match result {
                    Ok(()) => Status::Done,
                    Err(e) => Status::Failed(e),
                });
            });
        if let Err(e) = spawned {
            import.set(Status::Failed(format!("could not start: {e}")));
        }
        import
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .map_or_else(|e| e.into_inner().clone(), |s| s.clone())
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn set(&self, status: Status) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
    }

    fn run(&self, url: Option<String>, folder: &Path, resources: &Path) -> Result<(), String> {
        let archive = match url {
            Some(url) => {
                let to = folder.join(ARCHIVE_NAME);
                download(&url, &to, &self.cancel, |got, total| {
                    self.set(Status::Downloading(got, total));
                })?;
                to
            }
            None => find_archive(folder).ok_or("no zip with the game files was found")?,
        };
        extract(&archive, resources, &self.cancel, |done, all| {
            self.set(Status::Unpacking(done, all));
        })?;
        // The zip is the player's copy of Mojang's files twice over: do not leave it behind.
        let _ = std::fs::remove_file(&archive);
        exclude_from_backup(resources);
        Ok(())
    }
}

/// `text` as a link to download: https only, and no spaces or line breaks.
pub fn download_link(text: &str) -> Option<String> {
    let text = text.trim();
    (text.len() <= 2048
        && text.starts_with("https://")
        && !text.contains(char::is_whitespace)
        && text.len() > "https://x".len())
    .then(|| text.to_owned())
}

/// A zip in `folder` holding prepared game files: `horizon-resources.zip` first, else any `.zip`
/// whose entries include `horizon-resources.json`.
pub fn find_archive(folder: &Path) -> Option<PathBuf> {
    let named = folder.join(ARCHIVE_NAME);
    if named.is_file() {
        return Some(named);
    }
    let mut zips: Vec<PathBuf> = std::fs::read_dir(folder)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")))
        .collect();
    zips.sort();
    zips.into_iter().find(|p| {
        std::fs::File::open(p)
            .ok()
            .and_then(|f| zip::ZipArchive::new(f).ok())
            .is_some_and(|z| marker_prefix(z.file_names().filter_map(Result::ok)).is_some())
    })
}

/// The folder the marker file sits in inside the archive (`""` at the root, `"resources/"` in a
/// folder of that name): what to strip from every entry.
fn marker_prefix<S: AsRef<str>>(names: impl Iterator<Item = S>) -> Option<String> {
    names
        .filter_map(|n| n.as_ref().strip_suffix(MARKER).map(str::to_owned))
        .filter(|prefix| (prefix.is_empty() || prefix.ends_with('/')) && !prefix.starts_with("__MACOSX"))
        // The shallowest one: a nested copy deeper in is not the root.
        .min_by_key(|p| p.matches('/').count())
}

/// Download `url` to `to`, reporting progress. Written to a `.part` file and renamed, so a
/// cut connection never leaves something that looks finished.
pub fn download(
    url: &str,
    to: &Path,
    cancel: &AtomicBool,
    progress: impl Fn(u64, Option<u64>),
) -> Result<(), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(20))
        .timeout_read(std::time::Duration::from_secs(60))
        .redirects(8)
        .user_agent("Horizon-iOS")
        .build();
    let response = agent.get(url).call().map_err(|e| match e {
        ureq::Error::Status(code, _) => format!("the server answered {code}"),
        other => format!("could not download: {other}"),
    })?;
    let total = response
        .header("Content-Length")
        .and_then(|l| l.parse::<u64>().ok());
    if total.is_some_and(|t| t > MAX_DOWNLOAD) {
        return Err("the file is too large to be the game files".into());
    }
    let part = to.with_extension("zip.part");
    let mut out = std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
    let mut reader = response.into_reader();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut got = 0u64;
    let mut last = std::time::Instant::now();
    let result = (|| loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_owned());
        }
        let n = reader.read(&mut buffer).map_err(|e| format!("download failed: {e}"))?;
        if n == 0 {
            return Ok(());
        }
        got += n as u64;
        if got > MAX_DOWNLOAD {
            return Err("the file is too large to be the game files".into());
        }
        out.write_all(&buffer[..n])
            .map_err(|e| format!("could not save it (is the storage full?): {e}"))?;
        if last.elapsed() >= std::time::Duration::from_millis(200) {
            last = std::time::Instant::now();
            progress(got, total);
        }
    })();
    drop(out);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    if total.is_some_and(|t| t != got) {
        let _ = std::fs::remove_file(&part);
        return Err("the download was cut short; try again".into());
    }
    progress(got, total);
    std::fs::rename(&part, to).map_err(|e| format!("{}: {e}", to.display()))
}

/// Unpack `archive` into `resources`, whatever folder inside it holds `horizon-resources.json`.
/// It goes to `<resources>.partial` first and replaces `resources` only when everything is
/// there. Entries that would leave the folder are refused.
pub fn extract(
    archive: &Path,
    resources: &Path,
    cancel: &AtomicBool,
    progress: impl Fn(u64, u64),
) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("not a zip file: {e}"))?;
    // GitHub Actions' upload-artifact wraps horizon-resources.zip inside an outer zip: unwrap
    // it automatically when the outer zip holds a single inner .zip and no marker of its own.
    if marker_prefix(zip.file_names().filter_map(Result::ok)).is_none()
        && zip.len() == 1
        && zip
            .by_index(0)
            .ok()
            .and_then(|e| e.name().ok().map(|n| n.to_ascii_lowercase().ends_with(".zip")))
            .unwrap_or(false)
    {
        let inner_path = archive.with_extension("inner.zip");
        {
            let mut entry = zip.by_index(0).map_err(|e| format!("bad zip entry: {e}"))?;
            let mut out = std::fs::File::create(&inner_path)
                .map_err(|e| format!("{}: {e}", inner_path.display()))?;
            let mut buf = vec![0u8; 256 * 1024];
            let mut written = 0u64;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    let _ = std::fs::remove_file(&inner_path);
                    return Err("cancelled".into());
                }
                let n = entry.read(&mut buf).map_err(|e| format!("inner zip: {e}"))?;
                if n == 0 {
                    break;
                }
                written += n as u64;
                if written > MAX_DOWNLOAD {
                    let _ = std::fs::remove_file(&inner_path);
                    return Err("the zip unpacks to too much: it is not the game files".into());
                }
                out.write_all(&buf[..n])
                    .map_err(|e| format!("{}: {e}", inner_path.display()))?;
            }
        }
        drop(zip);
        let res = extract_single(&inner_path, resources, cancel, progress);
        let _ = std::fs::remove_file(&inner_path);
        return res;
    }
    drop(zip);
    extract_single(archive, resources, cancel, progress)
}

fn extract_single(
    archive: &Path,
    resources: &Path,
    cancel: &AtomicBool,
    progress: impl Fn(u64, u64),
) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("not a zip file: {e}"))?;
    let prefix = marker_prefix(zip.file_names().filter_map(Result::ok))
        .ok_or_else(|| format!("the zip has no {MARKER}: it is not the prepared game files"))?;
    let partial = {
        let mut name = resources.as_os_str().to_owned();
        name.push(".partial");
        PathBuf::from(name)
    };
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir_all(&partial).map_err(|e| format!("{}: {e}", partial.display()))?;
    let total = zip.len() as u64;
    let mut unpacked = 0u64;
    let mut buffer = vec![0u8; 256 * 1024];
    let result = (|| -> Result<(), String> {
        for i in 0..zip.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            let mut entry = zip.by_index(i).map_err(|e| format!("bad zip entry: {e}"))?;
            // `enclosed_name` refuses absolute paths and `..`.
            let Some(name) = entry.enclosed_name() else {
                return Err(format!("the zip holds an unsafe path: {}", entry.name().unwrap_or_default()));
            };
            let name = name.to_string_lossy().replace('\\', "/");
            let Some(relative) = name.strip_prefix(prefix.as_str()) else {
                continue;
            };
            if relative.is_empty() || name.starts_with("__MACOSX") {
                continue;
            }
            let target = partial.join(relative);
            if entry.is_dir() {
                std::fs::create_dir_all(&target)
                    .map_err(|e| format!("{}: {e}", target.display()))?;
            } else {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("{}: {e}", parent.display()))?;
                }
                let mut out = std::fs::File::create(&target)
                    .map_err(|e| format!("{}: {e}", target.display()))?;
                loop {
                    let n = entry
                        .read(&mut buffer)
                        .map_err(|e| format!("{name}: {e}"))?;
                    if n == 0 {
                        break;
                    }
                    unpacked += n as u64;
                    if unpacked > MAX_UNPACKED {
                        return Err("the zip unpacks to too much: it is not the game files".into());
                    }
                    out.write_all(&buffer[..n])
                        .map_err(|e| format!("could not save it (is the storage full?): {e}"))?;
                }
            }
            if i % 64 == 0 || i + 1 == total as usize {
                progress(i as u64 + 1, total);
            }
        }
        if !partial.join(MARKER).is_file() {
            return Err(format!("the zip has no {MARKER} at its top"));
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(e);
    }
    // `resources` held no finished files (that is why the game asked): it is replaced.
    if resources.exists() {
        std::fs::remove_dir_all(resources).map_err(|e| format!("{}: {e}", resources.display()))?;
    }
    std::fs::rename(&partial, resources).map_err(|e| format!("{}: {e}", resources.display()))
}

/// Keep `path` out of iCloud and device backups (iOS): the prepared files are gigabytes that
/// can be made again. Elsewhere nothing happens.
pub fn exclude_from_backup(path: &Path) {
    #[cfg(target_os = "ios")]
    horizon_client_input::ios_files::exclude_from_backup(path);
    #[cfg(not(target_os = "ios"))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
            let options = zip::write::SimpleFileOptions::default();
            for (name, data) in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
            writer.finish().unwrap();
        }
        bytes
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("horizon-import-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn links_are_https_without_spaces() {
        assert!(download_link("  https://example.com/a/horizon-resources.zip\n").is_some());
        assert!(download_link("http://example.com/a.zip").is_none());
        assert!(download_link("https://example.com/a b.zip").is_none());
        assert!(download_link("hello").is_none());
        assert!(download_link("https://").is_none());
    }

    #[test]
    fn the_marker_names_the_folder_to_strip() {
        let at = |names: &[&str]| marker_prefix(names.iter().copied());
        assert_eq!(at(&["horizon-resources.json", "assets/a"]).as_deref(), Some(""));
        assert_eq!(
            at(&["resources/", "resources/horizon-resources.json"]).as_deref(),
            Some("resources/")
        );
        assert_eq!(
            at(&["a/b/horizon-resources.json", "a/horizon-resources.json"]).as_deref(),
            Some("a/")
        );
        assert_eq!(at(&["resources/other-horizon-resources.json"]), None);
        assert_eq!(at(&["assets/a"]), None);
    }

    #[test]
    fn a_resources_folder_zip_unpacks_in_place() {
        let dir = temp("folder");
        let zip = dir.join(ARCHIVE_NAME);
        std::fs::write(
            &zip,
            zip_with(&[
                ("resources/horizon-resources.json", b"{}"),
                ("resources/assets/minecraft/a.txt", b"a"),
                ("__MACOSX/resources/._x", b"x"),
            ]),
        )
        .unwrap();
        let resources = dir.join("resources");
        // An unfinished folder from before is replaced.
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::write(resources.join("stale"), b"old").unwrap();
        assert_eq!(find_archive(&dir), Some(zip.clone()));
        extract(&zip, &resources, &AtomicBool::new(false), |_, _| {}).unwrap();
        assert!(resources.join("horizon-resources.json").is_file());
        assert_eq!(std::fs::read(resources.join("assets/minecraft/a.txt")).unwrap(), b"a");
        assert!(!resources.join("stale").exists());
        assert!(!dir.join("resources.partial").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_root_zip_and_unrelated_zips_are_told_apart() {
        let dir = temp("root");
        std::fs::write(dir.join("a-photos.zip"), zip_with(&[("photo.jpg", b"x")])).unwrap();
        std::fs::write(
            dir.join("prepared.zip"),
            zip_with(&[("horizon-resources.json", b"{}"), ("assets/x", b"x")]),
        )
        .unwrap();
        assert_eq!(find_archive(&dir), Some(dir.join("prepared.zip")));
        let resources = dir.join("resources");
        extract(&dir.join("prepared.zip"), &resources, &AtomicBool::new(false), |_, _| {})
            .unwrap();
        assert!(resources.join("assets/x").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn foreign_and_escaping_zips_are_refused_and_leave_nothing() {
        let dir = temp("refuse");
        let resources = dir.join("resources");
        let foreign = dir.join("foreign.zip");
        std::fs::write(&foreign, zip_with(&[("photo.jpg", b"x")])).unwrap();
        let err = extract(&foreign, &resources, &AtomicBool::new(false), |_, _| {}).unwrap_err();
        assert!(err.contains("not the prepared game files"), "{err}");
        let escaping = dir.join("escaping.zip");
        std::fs::write(
            &escaping,
            zip_with(&[("horizon-resources.json", b"{}"), ("../evil.txt", b"x")]),
        )
        .unwrap();
        let err = extract(&escaping, &resources, &AtomicBool::new(false), |_, _| {}).unwrap_err();
        assert!(err.contains("unsafe path"), "{err}");
        assert!(!resources.exists());
        assert!(!dir.join("resources.partial").exists());
        assert!(!dir.parent().unwrap().join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
