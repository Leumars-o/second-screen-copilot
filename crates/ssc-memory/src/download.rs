//! Model-file downloader: HTTPS (reqwest) from a Hugging Face-style `resolve` URL, pinned
//! revision, SHA-256 check, `.part` file resumed with HTTP `Range`, atomic rename into place.
//!
//! Used for the embedder ([`crate::models`]) and by `ssc-stt` for the whisper model, so neither
//! fastembed nor whisper.cpp ever touches the network. No token is sent: every pinned repo is
//! public, and the Hugging Face token cache is never read.
//!
//! Layout: `<root>/<spec.dir>/<file.path>`, with `<file.path>.part` while downloading. A file sits
//! at its final path only after its hash matched, so a later [`ensure`] trusts a final file whose
//! size matches without re-hashing it (that keeps start-up fast for the 1.6 GB whisper model).

use sha2::{Digest, Sha256};
use ssc_core::{Result, SscError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hugging Face, the source of every pinned model.
pub const HF_BASE_URL: &str = "https://huggingface.co";

/// One file of a model repo, pinned by hash and size.
#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    /// Path inside the repo (and inside the local model dir), e.g. `onnx/model.onnx`.
    pub path: &'static str,
    /// Lower-case hex SHA-256 of the file.
    pub sha256: &'static str,
    pub size: u64,
}

/// A set of files from one repo at one pinned revision.
#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    /// `owner/name` on Hugging Face.
    pub repo: &'static str,
    /// Full commit hash. Never a branch name: the hashes below are only valid for one commit.
    pub revision: &'static str,
    /// Directory under the models root. Include the short revision so a re-pin never trusts a
    /// file left by the previous pin (present files are trusted by size, see module docs).
    pub dir: &'static str,
    pub files: &'static [ModelFile],
}

impl ModelSpec {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// Download progress for one file. `done` counts bytes already on disk, resumed ones included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress<'a> {
    pub file: &'a str,
    pub done: u64,
    pub total: u64,
}

/// Progress sink. Called often (once per received chunk); throttle on the caller's side.
pub type ProgressFn<'a> = &'a (dyn Fn(Progress<'_>) + Send + Sync);

/// `~/.ssc/models` (under [`ssc_core::config::data_dir`]).
pub fn models_dir() -> PathBuf {
    ssc_core::config::data_dir().join("models")
}

/// Where a spec's files live under `root`.
pub fn spec_dir(root: &Path, spec: &ModelSpec) -> PathBuf {
    root.join(spec.dir)
}

/// True if every file of `spec` is in place under `root` (final path, pinned size). No network.
pub fn is_present(root: &Path, spec: &ModelSpec) -> bool {
    let dir = spec_dir(root, spec);
    spec.files.iter().all(|f| std::fs::metadata(dir.join(f.path)).is_ok_and(|m| m.is_file() && m.len() == f.size))
}

/// One process-wide lock: two callers ensuring the same model must not write the same `.part`.
static ENSURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Makes sure every file of `spec` is present and verified under `root`, downloading what is
/// missing from `base_url` (normally [`HF_BASE_URL`]). Returns the model directory.
///
/// A hash mismatch is an error and leaves no file behind. An interrupted download keeps its
/// `.part` file and resumes from it next time; if a resumed file then fails the hash, it is
/// discarded and downloaded once more from scratch.
pub async fn ensure(base_url: &str, root: &Path, spec: &ModelSpec, progress: ProgressFn<'_>) -> Result<PathBuf> {
    let _guard = ENSURE_LOCK.lock().await;
    let dir = spec_dir(root, spec);
    if is_present(root, spec) {
        return Ok(dir);
    }
    let client = reqwest::Client::builder()
        .user_agent(concat!("ssc-model-downloader/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| SscError::Unavailable(format!("http client: {e}")))?;
    for file in spec.files {
        let url = format!("{}/{}/resolve/{}/{}", base_url.trim_end_matches('/'), spec.repo, spec.revision, file.path);
        ensure_file(&client, &url, &dir.join(file.path), file, progress).await?;
    }
    Ok(dir)
}

async fn ensure_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    file: &ModelFile,
    progress: ProgressFn<'_>,
) -> Result<()> {
    match std::fs::metadata(dest) {
        Ok(m) if m.is_file() && m.len() == file.size => return Ok(()),
        Ok(_) => remove_quiet(dest),
        Err(_) => {}
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_err("create model dir", parent, e))?;
    }
    let part = part_path(dest);
    let resumed = part_len(&part) > 0;
    match fetch_and_verify(client, url, &part, file, progress).await {
        Err(SscError::Invalid(_)) if resumed => {
            // The kept prefix may have been bad (or from another revision): start over once.
            remove_quiet(&part);
            fetch_and_verify(client, url, &part, file, progress).await?;
        }
        r => r?,
    }
    std::fs::rename(&part, dest).map_err(|e| io_err("rename into place", dest, e))?;
    if let Some(parent) = dest.parent()
        && let Ok(d) = std::fs::File::open(parent)
    {
        let _ = d.sync_all(); // make the rename durable
    }
    Ok(())
}

/// Downloads into `part` (resuming from its current length) and checks size and hash. On any
/// verification failure (`SscError::Invalid`) the `.part` file is deleted; on a network error it
/// is kept for the next attempt.
async fn fetch_and_verify(
    client: &reqwest::Client,
    url: &str,
    part: &Path,
    file: &ModelFile,
    progress: ProgressFn<'_>,
) -> Result<()> {
    let r = fetch(client, url, part, file, progress).await.and_then(|()| verify(part, file));
    if matches!(r, Err(SscError::Invalid(_))) {
        remove_quiet(part);
    }
    r
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    part: &Path,
    file: &ModelFile,
    progress: ProgressFn<'_>,
) -> Result<()> {
    let mut have = part_len(part);
    if have > file.size {
        remove_quiet(part);
        have = 0;
    }
    if have == file.size {
        return Ok(()); // complete but not yet verified (e.g. crashed before the rename)
    }
    let mut req = client.get(url);
    if have > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let mut resp = req.send().await.map_err(|e| net_err(url, e))?;
    let status = resp.status();
    let mut out = if status == reqwest::StatusCode::PARTIAL_CONTENT && have > 0 {
        if content_range_start(&resp) != Some(have) {
            return Err(SscError::Unavailable(format!("{url}: server sent a different byte range")));
        }
        std::fs::OpenOptions::new().append(true).open(part).map_err(|e| io_err("open", part, e))?
    } else if status == reqwest::StatusCode::OK {
        have = 0; // server ignored the Range header: start over
        std::fs::File::create(part).map_err(|e| io_err("create", part, e))?
    } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        remove_quiet(part);
        return Err(SscError::Unavailable(format!("{url}: stale partial download discarded, retry")));
    } else {
        return Err(SscError::Unavailable(format!("{url}: HTTP {status}")));
    };
    progress(Progress { file: file.path, done: have, total: file.size });
    while let Some(chunk) = resp.chunk().await.map_err(|e| net_err(url, e))? {
        have += chunk.len() as u64;
        if have > file.size {
            return Err(SscError::Invalid(format!("{}: larger than the pinned {} bytes", file.path, file.size)));
        }
        out.write_all(&chunk).map_err(|e| io_err("write", part, e))?;
        progress(Progress { file: file.path, done: have, total: file.size });
    }
    out.sync_all().map_err(|e| io_err("sync", part, e))?;
    if have != file.size {
        // Connection closed early: keep the prefix and let the next attempt resume.
        return Err(SscError::Unavailable(format!("{}: download ended at {have} of {} bytes", file.path, file.size)));
    }
    Ok(())
}

fn verify(part: &Path, file: &ModelFile) -> Result<()> {
    let got = sha256_file(part)?;
    if !got.eq_ignore_ascii_case(file.sha256) {
        return Err(SscError::Invalid(format!("{}: sha256 mismatch (expected {}, got {got})", file.path, file.sha256)));
    }
    Ok(())
}

/// Lower-case hex SHA-256 of a file, streamed.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).map_err(|e| io_err("open", path, e))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| io_err("read", path, e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn content_range_start(resp: &reqwest::Response) -> Option<u64> {
    let v = resp.headers().get(reqwest::header::CONTENT_RANGE)?.to_str().ok()?;
    parse_content_range_start(v)
}

/// `bytes 100-199/200` → 100.
fn parse_content_range_start(v: &str) -> Option<u64> {
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    rest.split('-').next()?.trim().parse().ok()
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn part_len(part: &Path) -> u64 {
    std::fs::metadata(part).map(|m| m.len()).unwrap_or(0)
}

fn remove_quiet(p: &Path) {
    let _ = std::fs::remove_file(p);
}

fn io_err(what: &str, path: &Path, e: std::io::Error) -> SscError {
    SscError::Unavailable(format!("{what} {}: {e}", path.display()))
}

fn net_err(url: &str, e: reqwest::Error) -> SscError {
    // Drop the URL reqwest attaches: after a redirect it is a signed CDN link.
    SscError::Unavailable(format!("download {url}: {}", e.without_url()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parsing() {
        assert_eq!(parse_content_range_start("bytes 100-199/200"), Some(100));
        assert_eq!(parse_content_range_start("bytes 0-0/*"), Some(0));
        assert_eq!(parse_content_range_start("items 1-2/3"), None);
        assert_eq!(parse_content_range_start("bytes */200"), None);
    }

    #[test]
    fn part_path_appends_suffix() {
        assert_eq!(part_path(Path::new("/a/onnx/model.onnx")), PathBuf::from("/a/onnx/model.onnx.part"));
    }

    #[test]
    fn sha256_of_known_input() {
        let dir = std::env::temp_dir().join(format!("ssc-dl-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("abc");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
