use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const LOG_CAPACITY: usize = 2000;

#[derive(Debug, thiserror::Error)]
pub enum CollectorError {
    #[error("Collector binary not found: {}", path.display())]
    NotFound { path: PathBuf },
    #[error("failed to run {}: {source}", path.display())]
    Spawn {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("`{} {command}` failed: {detail}", path.display())]
    Probe {
        path: PathBuf,
        command: &'static str,
        detail: String,
    },
    #[error("the Collector build at {} has no {kind} named {wanted}, which otel-test requires", path.display())]
    MissingComponent {
        path: PathBuf,
        kind: &'static str,
        wanted: String,
    },
    #[error("the Collector rejected the rewritten configuration:\n{output}")]
    Invalid { output: String },
    #[error("the Collector exited during startup ({status}){}", format_logs(logs))]
    Exited {
        status: ExitStatus,
        logs: Vec<String>,
    },
    #[error("the Collector did not become ready within {:.1}s: {detail}{}", waited.as_secs_f32(), format_logs(logs))]
    NotReady {
        waited: Duration,
        detail: String,
        logs: Vec<String>,
    },
    #[error("failed to stop the Collector: {0}")]
    Stop(#[source] std::io::Error),
}

fn format_logs(logs: &[String]) -> String {
    const TAIL: usize = 20;
    const MAX_EARLIER: usize = 5;
    if logs.is_empty() {
        return "\n(no Collector output)".into();
    }
    let skip = logs.len().saturating_sub(TAIL);
    let earlier: Vec<&String> = logs[..skip]
        .iter()
        .filter(|l| is_error_line(l))
        .take(MAX_EARLIER)
        .collect();
    let mut s = String::new();
    if !earlier.is_empty() {
        s.push_str("\nErrors logged earlier:");
        for line in earlier {
            s.push_str("\n  | ");
            s.push_str(line);
        }
    }
    s.push_str(&format!(
        "\nCollector output (last {} lines):",
        logs.len() - skip
    ));
    for line in &logs[skip..] {
        s.push_str("\n  | ");
        s.push_str(line);
    }
    s
}

fn is_error_line(line: &str) -> bool {
    line.starts_with("Error:")
        || line
            .split('\t')
            .nth(1)
            .is_some_and(|level| matches!(level, "error" | "dpanic" | "panic" | "fatal"))
}

#[derive(Debug, Default, Deserialize)]
struct Components {
    #[serde(default)]
    receivers: Vec<ComponentEntry>,
    #[serde(default)]
    exporters: Vec<ComponentEntry>,
    #[serde(default)]
    extensions: Vec<ComponentEntry>,
}

#[derive(Debug, Deserialize)]
struct ComponentEntry {
    name: String,
}

#[derive(Debug)]
pub struct Collector {
    path: PathBuf,
    version: String,
    components: Components,
}

impl Collector {
    pub async fn open(path: &Path) -> Result<Self, CollectorError> {
        if !path.is_file() {
            return Err(CollectorError::NotFound {
                path: path.to_path_buf(),
            });
        }
        let absolute = std::path::absolute(path).map_err(|source| CollectorError::Spawn {
            path: path.to_path_buf(),
            source,
        })?;
        let path = absolute.as_path();
        let version_out = run(path, &["--version"], "--version").await?;
        let version = parse_version(&version_out)
            .ok_or_else(|| CollectorError::Probe {
                path: path.to_path_buf(),
                command: "--version",
                detail: format!("unexpected output {:?}", version_out.trim()),
            })?
            .to_owned();
        let components_out = run(path, &["components"], "components").await?;
        let components: Components =
            serde_yaml::from_str(&components_out).map_err(|e| CollectorError::Probe {
                path: path.to_path_buf(),
                command: "components",
                detail: format!("unparseable output: {e}"),
            })?;
        let collector = Collector {
            path: path.to_path_buf(),
            version,
            components,
        };
        if !collector.has(&collector.components.receivers, "otlp") {
            return Err(collector.missing("receiver", "otlp"));
        }
        collector.otlp_http_exporter()?;
        tracing::debug!(path = %path.display(), version = %collector.version, "probed Collector");
        Ok(collector)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn otlp_http_exporter(&self) -> Result<&'static str, CollectorError> {
        ["otlp_http", "otlphttp"]
            .into_iter()
            .find(|n| self.has(&self.components.exporters, n))
            .ok_or_else(|| self.missing("exporter", "otlp_http"))
    }

    pub fn otlp_grpc_exporter(&self) -> Result<&'static str, CollectorError> {
        ["otlp_grpc", "otlp"]
            .into_iter()
            .find(|n| self.has(&self.components.exporters, n))
            .ok_or_else(|| self.missing("exporter", "otlp_grpc"))
    }

    pub fn has_health_check(&self) -> bool {
        self.has(&self.components.extensions, "health_check")
    }

    fn has(&self, list: &[ComponentEntry], name: &str) -> bool {
        list.iter().any(|c| c.name == name)
    }

    fn missing(&self, kind: &'static str, wanted: &str) -> CollectorError {
        CollectorError::MissingComponent {
            path: self.path.clone(),
            kind,
            wanted: wanted.to_owned(),
        }
    }

    pub async fn validate(&self, config: &Path, cwd: &Path) -> Result<(), CollectorError> {
        let mut cmd = Command::new(&self.path);
        cmd.arg("validate").arg(config_arg(config)).current_dir(cwd);
        let out = output(&self.path, cmd, "validate").await?;
        if out.status.success() {
            return Ok(());
        }
        let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stdout));
        Err(CollectorError::Invalid {
            output: text.trim().to_owned(),
        })
    }

    pub async fn start(
        &self,
        config: &Path,
        cwd: &Path,
        probe: &ReadinessProbe,
    ) -> Result<RunningCollector, CollectorError> {
        let mut cmd = Command::new(&self.path);
        cmd.arg(config_arg(config))
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|source| CollectorError::Spawn {
            path: self.path.clone(),
            source,
        })?;
        tracing::debug!(pid = child.id(), config = %config.display(), "started Collector");

        let logs = LogBuffer::default();
        let readers = vec![
            logs.follow(child.stdout.take().expect("piped")),
            logs.follow(child.stderr.take().expect("piped")),
        ];
        let mut running = RunningCollector {
            child,
            logs,
            readers,
        };
        match probe.wait(&mut running).await {
            Ok(()) => Ok(running),
            Err(e) => {
                let _ = running.child.kill().await;
                Err(e)
            }
        }
    }
}

fn config_arg(config: &Path) -> String {
    format!("--config=file:{}", config.display())
}

async fn output(
    path: &Path,
    mut cmd: Command,
    command: &'static str,
) -> Result<std::process::Output, CollectorError> {
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    match tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(source)) => Err(CollectorError::Spawn {
            path: path.to_path_buf(),
            source,
        }),
        Err(_) => Err(CollectorError::Probe {
            path: path.to_path_buf(),
            command,
            detail: format!("timed out after {}s", PROBE_TIMEOUT.as_secs()),
        }),
    }
}

async fn run(path: &Path, args: &[&str], command: &'static str) -> Result<String, CollectorError> {
    let mut cmd = Command::new(path);
    cmd.args(args);
    let out = output(path, cmd, command).await?;
    if !out.status.success() {
        return Err(CollectorError::Probe {
            path: path.to_path_buf(),
            command,
            detail: format!(
                "{}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Debug, Clone)]
pub struct ReadinessProbe {
    pub tcp_ports: Vec<u16>,
    pub health_check_port: Option<u16>,
    pub timeout: Duration,
}

impl ReadinessProbe {
    async fn wait(&self, running: &mut RunningCollector) -> Result<(), CollectorError> {
        let started = Instant::now();
        let client = reqwest::Client::new();
        loop {
            if let Some(status) = running.child.try_wait().map_err(CollectorError::Stop)? {
                running.drain_logs().await;
                return Err(CollectorError::Exited {
                    status,
                    logs: running.logs(),
                });
            }
            let pending = self.pending(&client).await;
            if pending.is_empty() {
                tracing::debug!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "Collector ready"
                );
                return Ok(());
            }
            if started.elapsed() >= self.timeout {
                return Err(CollectorError::NotReady {
                    waited: started.elapsed(),
                    detail: pending.join("; "),
                    logs: running.logs(),
                });
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn pending(&self, client: &reqwest::Client) -> Vec<String> {
        let mut pending = Vec::new();
        for port in &self.tcp_ports {
            let connect = tokio::net::TcpStream::connect(("127.0.0.1", *port));
            if !matches!(
                tokio::time::timeout(Duration::from_millis(250), connect).await,
                Ok(Ok(_))
            ) {
                pending.push(format!("receiver port {port} not accepting connections"));
            }
        }
        if let Some(port) = self.health_check_port {
            let url = format!("http://127.0.0.1:{port}/");
            let ok = match client
                .get(&url)
                .timeout(Duration::from_millis(500))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    r.text().await.is_ok_and(|b| b.contains("Server available"))
                }
                _ => false,
            };
            if !ok {
                pending.push(format!(
                    "health check at {url} not reporting \"Server available\""
                ));
            }
        }
        pending
    }
}

#[derive(Debug, Clone, Default)]
struct LogBuffer(Arc<Mutex<VecDeque<String>>>);

impl LogBuffer {
    fn follow(&self, stream: impl AsyncRead + Unpin + Send + 'static) -> JoinHandle<()> {
        let buf = self.0.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut b = buf.lock().expect("log buffer poisoned");
                if b.len() == LOG_CAPACITY {
                    b.pop_front();
                }
                b.push_back(line);
            }
        })
    }

    fn snapshot(&self) -> Vec<String> {
        self.0
            .lock()
            .expect("log buffer poisoned")
            .iter()
            .cloned()
            .collect()
    }
}

#[derive(Debug)]
pub struct RunningCollector {
    child: Child,
    logs: LogBuffer,
    readers: Vec<JoinHandle<()>>,
}

impl RunningCollector {
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn logs(&self) -> Vec<String> {
        self.logs.snapshot()
    }

    pub fn exit_status(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub async fn stop(self, grace: Duration) -> Result<Stopped, CollectorError> {
        self.shutdown(grace).await.map(|(stopped, _)| stopped)
    }

    pub async fn shutdown(
        mut self,
        grace: Duration,
    ) -> Result<(Stopped, Vec<String>), CollectorError> {
        let stopped = self.wait_for_exit(grace).await?;
        self.drain_logs().await;
        Ok((stopped, self.logs()))
    }

    async fn wait_for_exit(&mut self, grace: Duration) -> Result<Stopped, CollectorError> {
        if let Some(status) = self.child.try_wait().map_err(CollectorError::Stop)? {
            return Ok(Stopped {
                status,
                how: StopKind::AlreadyExited,
            });
        }
        terminate(&mut self.child);
        let stopped = match tokio::time::timeout(grace, self.child.wait()).await {
            Ok(r) => Stopped {
                status: r.map_err(CollectorError::Stop)?,
                how: StopKind::OnRequest,
            },
            Err(_) => {
                tracing::warn!(
                    "Collector did not exit within {}s; killing it",
                    grace.as_secs_f32()
                );
                self.child.kill().await.map_err(CollectorError::Stop)?;
                Stopped {
                    status: self.child.wait().await.map_err(CollectorError::Stop)?,
                    how: StopKind::KilledAfterGrace,
                }
            }
        };
        Ok(stopped)
    }

    async fn drain_logs(&mut self) {
        for r in self.readers.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(2), r).await;
        }
    }
}

#[cfg(unix)]
fn terminate(child: &mut Child) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok()) {
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    }
}

#[cfg(not(unix))]
fn terminate(child: &mut Child) {
    let _ = child.start_kill();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    AlreadyExited,
    OnRequest,
    KilledAfterGrace,
}

#[derive(Debug, Clone, Copy)]
pub struct Stopped {
    pub status: ExitStatus,
    pub how: StopKind,
}

impl Stopped {
    pub fn check(&self) -> Result<(), String> {
        match self.how {
            StopKind::KilledAfterGrace => Err(
                "the Collector did not exit after the shutdown request and was killed".to_owned(),
            ),
            StopKind::OnRequest if cfg!(not(unix)) => Ok(()),
            _ if self.status.success() => Ok(()),
            _ => Err(format!("the Collector exited with {}", self.status)),
        }
    }
}

pub fn free_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

fn parse_version(out: &str) -> Option<String> {
    let re = regex::Regex::new(r"^\S+ version v?(\d+\.\d+\.\d+\S*)$").expect("valid regex");
    let first = out.lines().next()?.trim();
    re.captures(first).map(|c| c[1].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_output() {
        assert_eq!(
            parse_version("otelcol-contrib version 0.164.0\n").as_deref(),
            Some("0.164.0")
        );
        assert_eq!(
            parse_version("my-otelcol version v0.120.1-dev").as_deref(),
            Some("0.120.1-dev")
        );
        assert_eq!(parse_version("echo (uutils coreutils) 0.2.2"), None);
        assert_eq!(
            parse_version("echo (GNU coreutils) 9.4\nWritten by Brian Fox and Chet Ramey."),
            None
        );
        assert_eq!(parse_version("--version"), None);
        assert_eq!(parse_version(""), None);
    }

    #[tokio::test]
    async fn missing_binary() {
        let err = Collector::open(Path::new("/nonexistent/otelcol"))
            .await
            .unwrap_err();
        assert!(matches!(err, CollectorError::NotFound { .. }), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_binary_that_is_not_a_collector() {
        let err = Collector::open(Path::new("/bin/echo")).await.unwrap_err();
        assert!(
            matches!(
                err,
                CollectorError::Probe {
                    command: "--version",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn log_tail_formatting() {
        let logs: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        let s = format_logs(&logs);
        assert!(s.starts_with("\nCollector output (last 20 lines):"));
        assert!(s.contains("  | line 10") && s.contains("  | line 29") && !s.contains("line 9\n"));
        assert_eq!(format_logs(&[]), "\n(no Collector output)");

        let mut logs = vec![
            "2026-11-02T12:25:20.823+0530\tinfo\tservice.go:1\tStarting".to_owned(),
            "2026-11-02T12:25:20.823+0530\terror\tprocessor.go:127\tCould not initialize kube client".to_owned(),
        ];
        logs.extend((0..25).map(|i| format!("\tframe {i}")));
        let s = format_logs(&logs);
        assert!(
            s.starts_with("\nErrors logged earlier:\n  | 2026-11-02T12:25:20.823+0530\terror\tprocessor.go:127\tCould not initialize kube client\nCollector output (last 20 lines):"),
            "{s}"
        );
        assert!(!s.contains("Starting"));
        assert!(!format_logs(&logs[..1]).contains("Errors logged earlier"));
        assert!(is_error_line("Error: invalid configuration"));
        assert!(!is_error_line(
            "2026\twarn\tx.go:1\tmessage mentioning error"
        ));
    }

    #[test]
    fn free_ports_are_bindable() {
        let p = free_port().unwrap();
        std::net::TcpListener::bind(("127.0.0.1", p)).unwrap();
    }
}
