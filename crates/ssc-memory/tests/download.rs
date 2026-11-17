//! Model downloader against a local fake HTTP server (no real network), plus a `--features live`
//! test that fetches the real bge-small files and runs one embedding.

use sha2::{Digest, Sha256};
use ssc_core::SscError;
use ssc_memory::download::{self, ModelFile, ModelSpec, Progress};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const REPO: &str = "acme/tiny";
const REV: &str = "0123456789abcdef0123456789abcdef01234567";

/// (path, Range header).
type Request = (String, Option<String>);

#[derive(Default)]
struct Behaviour {
    /// Serve `206` for `Range` requests; otherwise ignore the header and send `200` + everything.
    honor_range: bool,
    /// Next response only: promise the full body but close after this many bytes.
    cut_after: Option<usize>,
}

#[derive(Clone)]
struct Fake {
    url: String,
    files: Arc<HashMap<String, Vec<u8>>>,
    behaviour: Arc<Mutex<Behaviour>>,
    /// (path, Range header) of every request.
    log: Arc<Mutex<Vec<Request>>>,
}

impl Fake {
    async fn start(files: &[(&str, &[u8])]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = Fake {
            url: format!("http://{}", listener.local_addr().unwrap()),
            files: Arc::new(files.iter().map(|(p, b)| (format!("/{REPO}/resolve/{REV}/{p}"), b.to_vec())).collect()),
            behaviour: Arc::new(Mutex::new(Behaviour { honor_range: true, cut_after: None })),
            log: Arc::default(),
        };
        let f = fake.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                tokio::spawn(f.clone().serve(sock));
            }
        });
        fake
    }

    async fn serve(self, mut sock: tokio::net::TcpStream) {
        let mut req = Vec::new();
        let mut buf = [0u8; 1024];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut buf).await.unwrap();
            if n == 0 {
                return;
            }
            req.extend_from_slice(&buf[..n]);
        }
        let req = String::from_utf8_lossy(&req).to_string();
        let path = req.split_whitespace().nth(1).unwrap().to_string();
        let range = req
            .lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("range")).map(|(_, v)| v.trim()))
            .map(str::to_string);
        self.log.lock().unwrap().push((path.clone(), range.clone()));
        let (honor, cut) = {
            let mut b = self.behaviour.lock().unwrap();
            (b.honor_range, b.cut_after.take())
        };
        let Some(body) = self.files.get(&path) else {
            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            return;
        };
        let start = range
            .as_deref()
            .filter(|_| honor)
            .and_then(|r| r.strip_prefix("bytes="))
            .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok());
        let (head, slice) = match start {
            Some(s) if s >= body.len() => {
                let h = format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                (h, &body[..0])
            }
            Some(s) => (
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {s}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len() - 1,
                    body.len(),
                    body.len() - s
                ),
                &body[s..],
            ),
            None => {
                (format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()), &body[..])
            }
        };
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.write_all(&slice[..cut.unwrap_or(slice.len()).min(slice.len())]).await;
        let _ = sock.flush().await;
    }

    fn requests(&self) -> Vec<Request> {
        self.log.lock().unwrap().clone()
    }

    fn set(&self, honor_range: bool, cut_after: Option<usize>) {
        *self.behaviour.lock().unwrap() = Behaviour { honor_range, cut_after };
    }
}

fn sha(b: &[u8]) -> &'static str {
    let h: String = Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect();
    Box::leak(h.into_boxed_str())
}

fn spec(files: Vec<ModelFile>) -> &'static ModelSpec {
    Box::leak(Box::new(ModelSpec { repo: REPO, revision: REV, dir: "tiny@0123", files: Box::leak(files.into()) }))
}

fn file(path: &'static str, body: &[u8]) -> ModelFile {
    ModelFile { path, sha256: sha(body), size: body.len() as u64 }
}

fn body(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn tmp_root(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ssc-dl-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn part(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".part");
    s.into()
}

fn no_progress(_: Progress<'_>) {}

#[tokio::test]
async fn downloads_verifies_and_second_call_is_offline() {
    let (a, b) = (body(10_000, 1), body(300, 2));
    let fake = Fake::start(&[("onnx/model.onnx", &a), ("tokenizer.json", &b)]).await;
    let spec = spec(vec![file("onnx/model.onnx", &a), file("tokenizer.json", &b)]);
    let root = tmp_root("ok");
    let last: Mutex<HashMap<String, (u64, u64)>> = Mutex::default();
    let progress = |p: Progress<'_>| {
        last.lock().unwrap().insert(p.file.to_string(), (p.done, p.total));
    };

    assert!(!download::is_present(&root, spec));
    let dir = download::ensure(&fake.url, &root, spec, &progress).await.unwrap();
    assert_eq!(dir, root.join("tiny@0123"));
    assert_eq!(std::fs::read(dir.join("onnx/model.onnx")).unwrap(), a);
    assert_eq!(std::fs::read(dir.join("tokenizer.json")).unwrap(), b);
    assert!(!part(&dir.join("onnx/model.onnx")).exists());
    assert_eq!(last.lock().unwrap()["onnx/model.onnx"], (10_000, 10_000));
    assert!(download::is_present(&root, spec));
    assert_eq!(fake.requests().len(), 2);
    assert!(fake.requests().iter().all(|(_, r)| r.is_none()));

    download::ensure(&fake.url, &root, spec, &no_progress).await.unwrap();
    assert_eq!(fake.requests().len(), 2, "present files must not be fetched again");
}

#[tokio::test]
async fn hash_mismatch_is_an_error_and_leaves_no_file() {
    let a = body(5_000, 3);
    let fake = Fake::start(&[("model.bin", &a)]).await;
    let spec = spec(vec![ModelFile { path: "model.bin", sha256: sha(b"something else"), size: a.len() as u64 }]);
    let root = tmp_root("mismatch");
    let err = download::ensure(&fake.url, &root, spec, &no_progress).await.unwrap_err();
    assert!(matches!(err, SscError::Invalid(ref m) if m.contains("sha256 mismatch")), "{err:?}");
    let dest = root.join("tiny@0123/model.bin");
    assert!(!dest.exists());
    assert!(!part(&dest).exists());
}

#[tokio::test]
async fn oversized_body_is_rejected_and_removed() {
    let a = body(5_000, 4);
    let fake = Fake::start(&[("model.bin", &a)]).await;
    let spec = spec(vec![ModelFile { path: "model.bin", sha256: sha(&a[..100]), size: 100 }]);
    let root = tmp_root("oversize");
    let err = download::ensure(&fake.url, &root, spec, &no_progress).await.unwrap_err();
    assert!(matches!(err, SscError::Invalid(_)), "{err:?}");
    assert!(!part(&root.join("tiny@0123/model.bin")).exists());
}

#[tokio::test]
async fn missing_file_is_unavailable() {
    let fake = Fake::start(&[]).await;
    let spec = spec(vec![file("model.bin", b"x")]);
    let root = tmp_root("404");
    let err = download::ensure(&fake.url, &root, spec, &no_progress).await.unwrap_err();
    assert!(matches!(err, SscError::Unavailable(ref m) if m.contains("404")), "{err:?}");
    assert!(!root.join("tiny@0123/model.bin").exists());
}

#[tokio::test]
async fn existing_partial_file_is_resumed_with_range() {
    let a = body(20_000, 5);
    let fake = Fake::start(&[("model.bin", &a)]).await;
    let spec = spec(vec![file("model.bin", &a)]);
    let root = tmp_root("resume");
    let dest = root.join("tiny@0123/model.bin");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(part(&dest), &a[..7_000]).unwrap();

    download::ensure(&fake.url, &root, spec, &no_progress).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), a);
    assert_eq!(fake.requests(), vec![(format!("/{REPO}/resolve/{REV}/model.bin"), Some("bytes=7000-".into()))]);
}

#[tokio::test]
async fn interrupted_download_keeps_part_and_next_call_resumes() {
    let a = body(50_000, 6);
    let fake = Fake::start(&[("model.bin", &a)]).await;
    let spec = spec(vec![file("model.bin", &a)]);
    let root = tmp_root("interrupt");
    let dest = root.join("tiny@0123/model.bin");

    fake.set(true, Some(12_345));
    let err = download::ensure(&fake.url, &root, spec, &no_progress).await.unwrap_err();
    assert!(matches!(err, SscError::Unavailable(_)), "{err:?}");
    assert!(!dest.exists());
    assert_eq!(std::fs::metadata(part(&dest)).unwrap().len(), 12_345);

    download::ensure(&fake.url, &root, spec, &no_progress).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), a);
    assert_eq!(fake.requests()[1].1.as_deref(), Some("bytes=12345-"));
}

#[tokio::test]
async fn server_ignoring_range_restarts_from_zero() {
    let a = body(9_000, 7);
    let fake = Fake::start(&[("model.bin", &a)]).await;
    fake.set(false, None);
    let spec = spec(vec![file("model.bin", &a)]);
    let root = tmp_root("norange");
    let dest = root.join("tiny@0123/model.bin");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(part(&dest), &a[..4_000]).unwrap();

    download::ensure(&fake.url, &root, spec, &no_progress).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), a, "200 must truncate, not append");
}

#[tokio::test]
async fn corrupt_partial_file_is_redone_once_from_scratch() {
    let a = body(8_000, 8);
    let fake = Fake::start(&[("model.bin", &a)]).await;
    let spec = spec(vec![file("model.bin", &a)]);
    let root = tmp_root("corrupt");
    let dest = root.join("tiny@0123/model.bin");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(part(&dest), vec![0xEE; 3_000]).unwrap();

    download::ensure(&fake.url, &root, spec, &no_progress).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), a);
    let ranges: Vec<_> = fake.requests().into_iter().map(|(_, r)| r).collect();
    assert_eq!(ranges, vec![Some("bytes=3000-".to_string()), None]);
}

#[tokio::test]
async fn wrong_size_final_file_is_replaced() {
    let a = body(1_000, 9);
    let fake = Fake::start(&[("model.bin", &a)]).await;
    let spec = spec(vec![file("model.bin", &a)]);
    let root = tmp_root("wrongsize");
    let dest = root.join("tiny@0123/model.bin");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(&dest, b"stale").unwrap();

    download::ensure(&fake.url, &root, spec, &no_progress).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), a);
}

/// Real download from Hugging Face (~130 MB, once into `~/.ssc/models`), then one embedding.
#[cfg(feature = "live")]
#[tokio::test]
async fn live_bge_small_download_and_embed() {
    use ssc_core::traits::Embedder;
    let dir = ssc_memory::models::ensure_embedder(&|p| {
        if p.done == p.total {
            eprintln!("{}: {} bytes", p.file, p.total);
        }
    })
    .await
    .unwrap();
    for f in ssc_memory::models::BGE_SMALL_EN_V15.files {
        assert_eq!(download::sha256_file(&dir.join(f.path)).unwrap(), f.sha256, "{}", f.path);
    }
    let e =
        tokio::task::spawn_blocking(move || ssc_memory::embed::FastEmbedder::from_dir(&dir).unwrap()).await.unwrap();
    let v = e.embed(&["The quarterly revenue grew by twelve percent.".to_string()]).unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].len(), 384);
    let norm: f32 = v[0].iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-3, "embedding should be L2-normalised, norm {norm}");
}
