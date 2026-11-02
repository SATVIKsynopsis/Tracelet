use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

pub const PINNED_VERSION: &str = "0.164.0";
const RELEASE_BASE_URL: &str =
    "https://github.com/open-telemetry/opentelemetry-collector-releases/releases/download";
const MAX_ARCHIVE_BYTES: u64 = 1 << 30;

const CHECKSUMS: &[(&str, &str)] = &[
    (
        "darwin_amd64",
        "b53ab416e9a90625c1453692f1d3a3c4eba72904424e31516866f73064b65d4f",
    ),
    (
        "darwin_arm64",
        "af6a0c56886e3cc36ccebe849fa167bad2bc47a97a48fd8497a0d6f93d0cf73a",
    ),
    (
        "linux_386",
        "df7b80391bb2c16bacd5de5506e5e0421b6470fa673598fc13d6ae317bf68189",
    ),
    (
        "linux_amd64",
        "f0c1ce9f661399beb4a58b711924716e08ea70c3802db0715fcfde3cbbada2d4",
    ),
    (
        "linux_arm64",
        "8bd4a7f443405522a2175cc1a8675f9e99cc047c7181b73461efd08efbba38d3",
    ),
    (
        "linux_armv7",
        "e4607793a9d7dcc90426312b180635bf6a2963993d12dc5e0488a573a966be83",
    ),
    (
        "linux_ppc64le",
        "adb03c765d98d77c24935811fa33c0a5525225e0e93079b044e4a8f174fe80d3",
    ),
    (
        "linux_riscv64",
        "602e68fc3772cf6030eeb67f640417e6bde9fedd18801aaf514e0a76b50a066f",
    ),
    (
        "linux_s390x",
        "821b2be5c0d5a7c96cdcee4b4743ec0a7c770707ea7cedd083e6b6a03dd3413b",
    ),
    (
        "windows_386",
        "d2738e412adb6cf125d9606a9b1d47a2294e0c6fb4431ad787d42feb79b05ee4",
    ),
    (
        "windows_amd64",
        "1fb8163f9036126a1bdc59eab2d7aef4e6cda36f6c1e618f2359663cae8b0f4a",
    ),
    (
        "windows_arm64",
        "850b67d16de82a85355bc94bf0dab71cce635d90fbe1e47cf2c5727dd99e1952",
    ),
];

#[derive(Debug, thiserror::Error)]
pub enum BinaryError {
    #[error(
        "no pinned Collector release for {os}/{arch}; install otelcol-contrib yourself and pass --collector-bin"
    )]
    UnsupportedPlatform {
        os: &'static str,
        arch: &'static str,
    },
    #[error("could not determine a cache directory; set OTEL_TEST_CACHE_DIR")]
    NoCacheDir,
    #[error(
        "failed to download {url}: {source}\nhint: check network access, set OTEL_TEST_DOWNLOAD_BASE_URL to a mirror, or pass --collector-bin"
    )]
    Download {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("Collector {PINNED_VERSION} is not cached at {} and --offline forbids downloading it", expected.display())]
    Offline { expected: PathBuf },
    #[error("download of {url} exceeded {limit} bytes")]
    TooLarge { url: String, limit: u64 },
    #[error(
        "checksum mismatch for {url}\n  expected sha256 {expected}\n  actual   sha256 {actual}\nthe download was discarded"
    )]
    ChecksumMismatch {
        url: String,
        expected: String,
        actual: String,
    },
    #[error("archive {url} does not contain a regular file named {name}")]
    MissingInArchive { url: String, name: String },
    #[error("{context} {}: {source}", path.display())]
    Io {
        context: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn io_err<'a>(
    context: &'static str,
    path: &'a Path,
) -> impl FnOnce(std::io::Error) -> BinaryError + 'a {
    move |source| BinaryError::Io {
        context,
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    pub platform: String,
    pub url: String,
    pub sha256: String,
}

impl Release {
    pub fn pinned() -> Result<Self, BinaryError> {
        let platform = current_platform()?;
        let sha256 = CHECKSUMS
            .iter()
            .find(|(p, _)| *p == platform)
            .map(|(_, s)| s.to_string())
            .ok_or(BinaryError::UnsupportedPlatform {
                os: std::env::consts::OS,
                arch: std::env::consts::ARCH,
            })?;
        let base = std::env::var("OTEL_TEST_DOWNLOAD_BASE_URL")
            .unwrap_or_else(|_| RELEASE_BASE_URL.to_owned());
        let base = base.trim_end_matches('/');
        Ok(Release {
            version: PINNED_VERSION.to_owned(),
            url: format!(
                "{base}/v{PINNED_VERSION}/otelcol-contrib_{PINNED_VERSION}_{platform}.tar.gz"
            ),
            platform,
            sha256,
        })
    }

    pub fn cached_path(&self, cache: &Path) -> PathBuf {
        cache
            .join("otelcol-contrib")
            .join(format!("{}-{}", self.version, self.platform))
            .join(executable_name())
    }
}

fn executable_name() -> &'static str {
    if cfg!(windows) {
        "otelcol-contrib.exe"
    } else {
        "otelcol-contrib"
    }
}

fn current_platform() -> Result<String, BinaryError> {
    use std::env::consts::{ARCH, OS};
    let unsupported = || BinaryError::UnsupportedPlatform { os: OS, arch: ARCH };
    let os = match OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        _ => return Err(unsupported()),
    };
    let arch = match ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "arm" => "armv7",
        "powerpc64" if cfg!(target_endian = "little") => "ppc64le",
        "riscv64" => "riscv64",
        "s390x" => "s390x",
        _ => return Err(unsupported()),
    };
    Ok(format!("{os}_{arch}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinarySource {
    Explicit,
    Environment,
    Cached,
    Downloaded,
}

pub async fn resolve(
    explicit: Option<&Path>,
    offline: bool,
    before_download: impl FnOnce(&Release),
) -> Result<(PathBuf, BinarySource), BinaryError> {
    if let Some(p) = explicit {
        return Ok((p.to_path_buf(), BinarySource::Explicit));
    }
    if let Some(p) = std::env::var_os("OTEL_TEST_COLLECTOR_BIN").filter(|v| !v.is_empty()) {
        return Ok((PathBuf::from(p), BinarySource::Environment));
    }
    let release = Release::pinned()?;
    let cache = cache_dir()?;
    let cached = release.cached_path(&cache);
    if cached.is_file() {
        return Ok((cached, BinarySource::Cached));
    }
    if offline {
        return Err(BinaryError::Offline { expected: cached });
    }
    before_download(&release);
    Ok((install(&release, &cache).await?, BinarySource::Downloaded))
}

pub fn cache_dir() -> Result<PathBuf, BinaryError> {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(dir) = env("OTEL_TEST_CACHE_DIR") {
        return Ok(dir);
    }
    let base = if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Caches"))
    } else if cfg!(windows) {
        env("LOCALAPPDATA")
    } else {
        env("XDG_CACHE_HOME").or_else(|| env("HOME").map(|h| h.join(".cache")))
    };
    base.map(|b| b.join("otel-test"))
        .ok_or(BinaryError::NoCacheDir)
}

pub async fn install(release: &Release, cache: &Path) -> Result<PathBuf, BinaryError> {
    let target = release.cached_path(cache);
    if target.is_file() {
        return Ok(target);
    }
    let dir = target.parent().expect("cached_path has a parent");
    std::fs::create_dir_all(dir).map_err(io_err("failed to create cache directory", dir))?;

    let archive = tempfile::NamedTempFile::new_in(dir)
        .map_err(io_err("failed to create temporary file in", dir))?;
    let actual = download(&release.url, archive.as_file()).await?;
    if !actual.eq_ignore_ascii_case(&release.sha256) {
        return Err(BinaryError::ChecksumMismatch {
            url: release.url.clone(),
            expected: release.sha256.clone(),
            actual,
        });
    }
    tracing::debug!(url = %release.url, sha256 = %actual, "archive checksum verified");

    let url = release.url.clone();
    let target_clone = target.clone();
    tokio::task::spawn_blocking(move || extract(archive.path(), &url, &target_clone))
        .await
        .expect("extraction task panicked")?;
    Ok(target)
}

async fn download(url: &str, mut file: &std::fs::File) -> Result<String, BinaryError> {
    let err = |source| BinaryError::Download {
        url: url.to_owned(),
        source,
    };
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .user_agent(concat!("otel-test/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(err)?;
    let mut response = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(err)?;
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    while let Some(chunk) = response.chunk().await.map_err(err)? {
        total += chunk.len() as u64;
        if total > MAX_ARCHIVE_BYTES {
            return Err(BinaryError::TooLarge {
                url: url.to_owned(),
                limit: MAX_ARCHIVE_BYTES,
            });
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .map_err(io_err("failed to write download to", Path::new(url)))?;
    }
    file.flush()
        .map_err(io_err("failed to write download to", Path::new(url)))?;
    Ok(hex(&hasher.finalize()))
}

fn extract(archive: &Path, url: &str, target: &Path) -> Result<(), BinaryError> {
    let name = executable_name();
    let file = std::fs::File::open(archive).map_err(io_err("failed to open", archive))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let read_err = io_err("failed to read archive", archive);
    for entry in tar.entries().map_err(read_err)? {
        let mut entry = entry.map_err(io_err("failed to read archive", archive))?;
        let path = entry
            .path()
            .map_err(io_err("failed to read archive", archive))?;
        let mut parts = path
            .components()
            .filter(|c| !matches!(c, std::path::Component::CurDir));
        let is_target =
            matches!((parts.next(), parts.next()), (Some(p), None) if p.as_os_str() == name);
        if !is_target || !entry.header().entry_type().is_file() {
            continue;
        }
        let dir = target.parent().expect("target has a parent");
        let mut out = tempfile::NamedTempFile::new_in(dir)
            .map_err(io_err("failed to create temporary file in", dir))?;
        std::io::copy(&mut entry, &mut out).map_err(io_err("failed to extract", target))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            out.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o755))
                .map_err(io_err("failed to set permissions on", target))?;
        }
        out.persist(target).map_err(|e| BinaryError::Io {
            context: "failed to move executable into cache at",
            path: target.to_path_buf(),
            source: e.error,
        })?;
        return Ok(());
    }
    Err(BinaryError::MissingInArchive {
        url: url.to_owned(),
        name: name.to_owned(),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve(body: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });
        format!("http://{addr}")
    }

    enum Entry<'a> {
        File(&'a str, &'a [u8]),
        Symlink(&'a str, &'a str),
    }

    fn tar_gz(entries: &[Entry]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for e in entries {
            let mut header = tar::Header::new_gnu();
            match e {
                Entry::File(path, data) => {
                    header.set_size(data.len() as u64);
                    header.set_mode(0o644);
                    header.set_entry_type(tar::EntryType::Regular);
                    builder.append_data(&mut header, path, *data).unwrap();
                }
                Entry::Symlink(path, target) => {
                    header.set_size(0);
                    header.set_entry_type(tar::EntryType::Symlink);
                    builder.append_link(&mut header, path, target).unwrap();
                }
            }
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn sha(data: &[u8]) -> String {
        hex(&Sha256::digest(data))
    }

    async fn release_for(archive: Vec<u8>, sha256: String) -> Release {
        Release {
            version: "9.9.9".into(),
            platform: "test_arch".into(),
            url: format!("{}/otelcol.tar.gz", serve(archive).await),
            sha256,
        }
    }

    fn cache_files(cache: &Path) -> Vec<String> {
        let dir = cache.join("otelcol-contrib/9.9.9-test_arch");
        let Ok(rd) = std::fs::read_dir(dir) else {
            return vec![];
        };
        rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn installs_verified_executable() {
        let archive = tar_gz(&[
            Entry::File("README.md", b"readme"),
            Entry::File("sub/otelcol-contrib", b"wrong file"),
            Entry::File(&format!("./{}", executable_name()), b"#!/bin/sh\necho ok\n"),
        ]);
        let release = release_for(archive.clone(), sha(&archive).to_uppercase()).await;
        let cache = tempfile::tempdir().unwrap();

        let path = install(&release, cache.path()).await.unwrap();
        assert_eq!(path, release.cached_path(cache.path()));
        assert_eq!(std::fs::read(&path).unwrap(), b"#!/bin/sh\necho ok\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        assert_eq!(cache_files(cache.path()), [executable_name()]);

        let offline = Release {
            url: "http://127.0.0.1:1/unreachable".into(),
            ..release
        };
        assert_eq!(install(&offline, cache.path()).await.unwrap(), path);
    }

    #[tokio::test]
    async fn rejects_checksum_mismatch_without_extracting() {
        let archive = tar_gz(&[Entry::File(executable_name(), b"evil")]);
        let release = release_for(archive, "00".repeat(32)).await;
        let cache = tempfile::tempdir().unwrap();

        let err = install(&release, cache.path()).await.unwrap_err();
        assert!(matches!(err, BinaryError::ChecksumMismatch { .. }), "{err}");
        assert!(err.to_string().contains("expected sha256 0000"), "{err}");
        assert!(
            cache_files(cache.path()).is_empty(),
            "nothing may remain in the cache"
        );
    }

    #[tokio::test]
    async fn rejects_archive_without_regular_executable() {
        let archive = tar_gz(&[
            Entry::Symlink(executable_name(), "/bin/sh"),
            Entry::File("bin/otelcol-contrib", b"nested"),
        ]);
        let release = release_for(archive.clone(), sha(&archive)).await;
        let cache = tempfile::tempdir().unwrap();

        let err = install(&release, cache.path()).await.unwrap_err();
        assert!(matches!(err, BinaryError::MissingInArchive { .. }), "{err}");
        assert!(cache_files(cache.path()).is_empty());
    }

    #[tokio::test]
    async fn reports_unreachable_server() {
        let release = Release {
            version: "9.9.9".into(),
            platform: "test_arch".into(),
            url: "http://127.0.0.1:1/otelcol.tar.gz".into(),
            sha256: "00".repeat(32),
        };
        let cache = tempfile::tempdir().unwrap();
        let err = install(&release, cache.path()).await.unwrap_err();
        assert!(matches!(err, BinaryError::Download { .. }), "{err}");
        assert!(err.to_string().contains("--collector-bin"), "{err}");
    }

    #[test]
    fn pinned_release_matches_current_platform() {
        let r = Release::pinned().expect("CI platforms are supported");
        assert_eq!(r.version, PINNED_VERSION);
        assert!(
            r.url.ends_with(&format!(
                "otelcol-contrib_{PINNED_VERSION}_{}.tar.gz",
                r.platform
            )),
            "{}",
            r.url
        );
        assert_eq!(r.sha256.len(), 64);
        assert!(
            CHECKSUMS
                .iter()
                .all(|(_, s)| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        );
    }
}
