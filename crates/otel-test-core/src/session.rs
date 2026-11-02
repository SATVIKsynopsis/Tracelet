use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::collector::{self, Collector, CollectorError, ReadinessProbe, RunningCollector};
use crate::config::CollectorConfig;
use otel_test_capture::CaptureServer;

use crate::rewrite::{self, CaptureTarget, RewriteError, RewriteOptions, Rewritten};

pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CaptureProtocol {
    #[default]
    Http,
    Grpc,
}

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error(transparent)]
    Rewrite(#[from] RewriteError),
    #[error("failed to write rewritten configuration to {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Collector(#[from] CollectorError),
}

#[derive(Debug)]
pub struct Prepared {
    dir: tempfile::TempDir,
    pub config_path: PathBuf,
    pub cwd: PathBuf,
    pub rewritten: Rewritten,
}

impl Prepared {
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    pub async fn start(
        &self,
        collector: &Collector,
        timeout: Duration,
    ) -> Result<RunningCollector, CollectorError> {
        let probe = ReadinessProbe {
            tcp_ports: self.rewritten.injections.iter().map(|i| i.port).collect(),
            health_check_port: self.rewritten.health_check_port,
            timeout,
        };
        collector.start(&self.config_path, &self.cwd, &probe).await
    }
}

pub async fn prepare(
    config: &CollectorConfig,
    collector: &Collector,
    capture: &CaptureServer,
    protocol: CaptureProtocol,
) -> Result<Prepared, PrepareError> {
    let target = match protocol {
        CaptureProtocol::Http => CaptureTarget::Http {
            base_url: capture.http_base_url(),
            exporter_type: collector.otlp_http_exporter()?.to_owned(),
        },
        CaptureProtocol::Grpc => CaptureTarget::Grpc {
            endpoint: capture.grpc_endpoint(),
            exporter_type: collector.otlp_grpc_exporter()?.to_owned(),
        },
    };
    let options = RewriteOptions {
        capture: target,
        health_check: collector.has_health_check(),
    };
    let rewritten = rewrite::rewrite(config, &options, distinct_ports())?;
    for x in &rewritten.exporters {
        capture.store().register(&x.slug, &x.original);
    }

    let dir = tempfile::Builder::new()
        .prefix("otel-test-")
        .tempdir()
        .map_err(|source| PrepareError::Write {
            path: std::env::temp_dir(),
            source,
        })?;
    let config_path = dir.path().join("collector.yaml");
    write_private(&config_path, &rewritten.yaml).map_err(|source| PrepareError::Write {
        path: config_path.clone(),
        source,
    })?;

    let cwd = match config.source.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    collector.validate(&config_path, &cwd).await?;
    tracing::debug!(path = %config_path.display(), "rewritten configuration validated");
    Ok(Prepared {
        dir,
        config_path,
        cwd,
        rewritten,
    })
}

fn distinct_ports() -> impl FnMut() -> std::io::Result<u16> {
    let mut seen = HashSet::new();
    move || {
        for _ in 0..100 {
            let p = collector::free_port()?;
            if seen.insert(p) {
                return Ok(p);
            }
        }
        Err(std::io::Error::other(
            "the OS kept returning already-allocated ports",
        ))
    }
}

fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(contents.as_bytes())
}
