mod init;
mod output;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Args, Parser, Subcommand, ValueEnum};
use otel_test_capture::{CaptureError, CaptureServer};
use otel_test_core::binary::{self, BinaryError, BinarySource};
use otel_test_core::collector::{Collector, CollectorError};
use otel_test_core::config::{CollectorConfig, ConfigError};
use otel_test_core::runner::{self, Outcome, PlannedTest};
use otel_test_core::session::{self, CaptureProtocol, PrepareError, Prepared};
use otel_test_core::spec::{self, SpecError, TestCase, TestSuite};

use output::display_path;

mod exit {
    pub const ASSERTION_FAILED: u8 = 1;
    pub const INVALID_SPEC: u8 = 2;
    pub const INVALID_CONFIG: u8 = 3;
    pub const COLLECTOR_UNAVAILABLE: u8 = 4;
    pub const EXECUTION_ERROR: u8 = 5;
    pub const INTERRUPTED: u8 = 130;
}

#[derive(Parser)]
#[command(
    name = "otel-test",
    version,
    about = "Unit tests for OpenTelemetry Collector configurations"
)]
struct Cli {
    #[arg(short, long, global = true, default_value = "otel-test.yaml")]
    file: PathBuf,

    #[arg(long, global = true, value_enum, default_value_t = ColorWhen::Auto)]
    color: ColorWhen,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum ColorWhen {
    Auto,
    Always,
    Never,
}

#[derive(Subcommand)]
enum Command {
    Init {
        #[arg(default_value = ".")]
        dir: PathBuf,
        #[arg(long)]
        force: bool,
    },
    Test {
        #[arg(short, long)]
        verbose: bool,
        #[arg(long)]
        filter: Option<String>,
        #[command(flatten)]
        collector: CollectorArgs,
    },
    Validate {
        #[command(flatten)]
        collector: CollectorArgs,
    },
}

#[derive(Args)]
struct CollectorArgs {
    #[arg(long, value_name = "PATH")]
    collector_bin: Option<PathBuf>,
    #[arg(long)]
    offline: bool,
}

#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) code: u8,
    pub(crate) message: String,
}

impl Failure {
    pub(crate) fn new(code: u8, message: impl ToString) -> Self {
        Failure {
            code,
            message: message.to_string(),
        }
    }
}

impl From<SpecError> for Failure {
    fn from(e: SpecError) -> Self {
        Failure::new(exit::INVALID_SPEC, e)
    }
}

impl From<ConfigError> for Failure {
    fn from(e: ConfigError) -> Self {
        Failure::new(exit::INVALID_CONFIG, e)
    }
}

impl From<BinaryError> for Failure {
    fn from(e: BinaryError) -> Self {
        Failure::new(exit::COLLECTOR_UNAVAILABLE, e)
    }
}

impl From<CollectorError> for Failure {
    fn from(e: CollectorError) -> Self {
        let code = match e {
            CollectorError::Invalid { .. } => exit::INVALID_CONFIG,
            _ => exit::COLLECTOR_UNAVAILABLE,
        };
        Failure::new(code, e)
    }
}

impl From<CaptureError> for Failure {
    fn from(e: CaptureError) -> Self {
        Failure::new(exit::EXECUTION_ERROR, e)
    }
}

impl From<PrepareError> for Failure {
    fn from(e: PrepareError) -> Self {
        match e {
            PrepareError::Collector(e) => e.into(),
            PrepareError::Rewrite(e) => Failure::new(exit::INVALID_CONFIG, e),
            e @ PrepareError::Write { .. } => Failure::new(exit::EXECUTION_ERROR, e),
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.color {
        ColorWhen::Auto => {}
        ColorWhen::Always => anstream::ColorChoice::Always.write_global(),
        ColorWhen::Never => anstream::ColorChoice::Never.write_global(),
    }

    let result = tokio::select! {
        r = run(cli) => r,
        _ = tokio::signal::ctrl_c() => Err(Failure::new(exit::INTERRUPTED, "interrupted")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => {
            output::error(&f.message);
            ExitCode::from(f.code)
        }
    }
}

async fn run(cli: Cli) -> Result<(), Failure> {
    output::banner();
    match cli.command {
        Command::Init { dir, force } => {
            let written = init::init(&dir, force)?;
            for p in &written {
                anstream::println!("  created {}", display_path(p));
            }
            let spec = dir.join("otel-test.yaml");
            let hint = if dir == Path::new(".") {
                "otel-test test".to_owned()
            } else {
                format!("otel-test test -f {}", display_path(&spec))
            };
            anstream::println!("\nNext: run `{hint}`");
            Ok(())
        }
        Command::Validate { collector } => validate(&cli.file, &collector).await,
        Command::Test {
            filter,
            verbose,
            collector,
        } => test(&cli.file, filter.as_deref(), verbose, &collector).await,
    }
}

async fn validate(file: &Path, args: &CollectorArgs) -> Result<(), Failure> {
    let suite = spec::load(file)?;
    let config = CollectorConfig::load(&suite.collector_config)?;
    let collector = open_collector(args).await?;
    output::header(&collector, &suite, suite.tests.len(), true);
    output::ok("test specification is valid\n");

    let capture = CaptureServer::start().await?;
    let prepared = session::prepare(&config, &collector, &capture, CaptureProtocol::Http).await?;
    output::notes(&prepared);
    output::ok("rewritten Collector configuration passes `otelcol validate`");

    let tests: Vec<&TestCase> = suite.tests.iter().collect();
    let planned = plan(&tests, &config, &prepared, &suite)?;
    anstream::println!();
    output::plan(&planned);
    anstream::println!();
    output::ok("all fixtures, receivers and exporter references resolve");
    capture.shutdown().await;
    Ok(())
}

fn plan(
    tests: &[&TestCase],
    config: &CollectorConfig,
    prepared: &Prepared,
    suite: &TestSuite,
) -> Result<Vec<PlannedTest>, Failure> {
    runner::plan(tests, config, prepared).map_err(|problems| {
        let list: String = problems
            .iter()
            .map(|p| format!("\n  - {}: {}", p.location, p.message))
            .collect();
        Failure::new(
            exit::INVALID_SPEC,
            format!(
                "test specification {} does not match the Collector configuration:{list}",
                display_path(&suite.source)
            ),
        )
    })
}

async fn test(
    file: &Path,
    filter: Option<&str>,
    verbose: bool,
    args: &CollectorArgs,
) -> Result<(), Failure> {
    let started = Instant::now();
    let suite = spec::load(file)?;
    let selected = select(&suite, filter);
    if selected.is_empty() {
        return Err(Failure::new(
            exit::INVALID_SPEC,
            format!(
                "no tests match filter {:?} ({} test(s) defined)",
                filter.unwrap_or_default(),
                suite.tests.len()
            ),
        ));
    }
    let config = CollectorConfig::load(&suite.collector_config)?;
    let collector = open_collector(args).await?;
    output::header(&collector, &suite, selected.len(), verbose);

    let capture = CaptureServer::start().await?;
    let prepared = session::prepare(&config, &collector, &capture, CaptureProtocol::Http).await?;
    if verbose {
        output::notes(&prepared);
    }
    let planned = plan(&selected, &config, &prepared, &suite)?;

    let mut last_passed = true;
    let results = runner::run(&planned, &prepared, &collector, &capture, |r| {
        output::result(r, verbose);
        last_passed = r.outcome == Outcome::Passed;
    })
    .await;
    capture.shutdown().await;

    let count = |f: fn(&Outcome) -> bool| results.iter().filter(|r| f(&r.outcome)).count();
    let passed = count(|o| *o == Outcome::Passed);
    let failed = count(|o| *o == Outcome::Failed);
    let errors = count(|o| matches!(o, Outcome::Error(_)));
    let startup = results
        .iter()
        .any(|r| matches!(&r.outcome, Outcome::Error(e) if e.startup));
    output::summary(
        passed,
        failed,
        errors,
        last_passed,
        started.elapsed().as_secs_f64(),
    );

    if startup {
        Err(Failure::new(
            exit::COLLECTOR_UNAVAILABLE,
            "the Collector failed to start",
        ))
    } else if errors > 0 {
        Err(Failure::new(
            exit::EXECUTION_ERROR,
            format!("{errors} test(s) could not be executed"),
        ))
    } else if failed > 0 {
        Err(Failure::new(
            exit::ASSERTION_FAILED,
            format!("{failed} test(s) failed"),
        ))
    } else {
        Ok(())
    }
}

async fn open_collector(args: &CollectorArgs) -> Result<Collector, Failure> {
    let (path, source) = binary::resolve(args.collector_bin.as_deref(), args.offline, |release| {
        anstream::eprintln!(
            "Downloading otelcol-contrib {} ({}) and verifying its SHA-256…",
            release.version,
            release.platform
        );
    })
    .await?;
    if source == BinarySource::Downloaded {
        anstream::eprintln!("Installed {}", path.display());
    }
    Ok(Collector::open(&path).await?)
}

fn select<'a>(suite: &'a TestSuite, filter: Option<&str>) -> Vec<&'a TestCase> {
    let needle = filter.map(str::to_lowercase);
    suite
        .tests
        .iter()
        .filter(|t| {
            needle
                .as_ref()
                .is_none_or(|n| t.name.to_lowercase().contains(n))
        })
        .collect()
}
