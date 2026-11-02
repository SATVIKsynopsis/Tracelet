use std::path::Path;

use anstream::println;
use anstyle::{AnsiColor, Style};
use otel_test_core::collector::Collector;
use otel_test_core::fixture;
use otel_test_core::runner::{Outcome, PlannedTest, TestResult};
use otel_test_core::session::Prepared;
use otel_test_core::spec::TestSuite;

const PASS: Style = AnsiColor::Green.on_default().bold();
const FAIL: Style = AnsiColor::Red.on_default().bold();
const ERROR: Style = AnsiColor::Yellow.on_default().bold();
const WARN: Style = AnsiColor::Yellow.on_default();
const DIM: Style = Style::new().dimmed();
const BOLD: Style = Style::new().bold();

pub fn banner() {
    println!("{BOLD}otel-test v{}{BOLD:#}\n", env!("CARGO_PKG_VERSION"));
}

pub fn header(collector: &Collector, suite: &TestSuite, count: usize, show_path: bool) {
    if show_path {
        println!(
            "Collector: v{} {DIM}({}){DIM:#}",
            collector.version(),
            display_path(collector.path())
        );
    } else {
        println!("Collector: v{}", collector.version());
    }
    println!("Config:    {}", display_path(&suite.collector_config));
    println!("Tests:     {count}\n");
}

pub fn notes(prepared: &Prepared) {
    println!("Configuration rewrite:");
    for note in &prepared.rewritten.notes {
        println!("  {DIM}-{DIM:#} {note}");
    }
    println!();
}

pub fn ok(message: &str) {
    println!("{PASS}✓{PASS:#} {message}");
}

pub fn plan(planned: &[PlannedTest]) {
    for p in planned {
        println!(
            "  • {} {DIM}({} {} via {}, timeout {}ms){DIM:#}",
            p.test.name,
            p.fixture.item_count,
            fixture::signal_items(p.fixture.signal, p.fixture.item_count),
            p.receiver,
            p.test.timeout.as_millis()
        );
        for a in &p.test.expect {
            println!("      - {a}");
        }
        for w in &p.warnings {
            println!("      {WARN}warning:{WARN:#} {w}");
        }
    }
}

pub fn result(r: &TestResult, verbose: bool) {
    let mark = match r.outcome {
        Outcome::Passed => format!("{PASS}✓{PASS:#}"),
        Outcome::Failed => format!("{FAIL}✗{FAIL:#}"),
        Outcome::Error(_) => format!("{ERROR}!{ERROR:#}"),
    };
    if verbose {
        println!(
            "{mark} {} {DIM}({}ms){DIM:#}",
            r.name,
            r.duration.as_millis()
        );
    } else {
        println!("{mark} {}", r.name);
    }
    if let Outcome::Error(e) = &r.outcome {
        println!();
        for line in e.message.lines() {
            println!("    {ERROR}{line}{ERROR:#}");
        }
    }
    let mut blank = false;
    for a in &r.assertions {
        if a.passed && !verbose {
            continue;
        }
        if !a.passed && !blank {
            println!();
            blank = true;
        }
        if a.passed {
            println!("    {PASS}✓{PASS:#} {}", a.assertion);
        } else {
            println!("    {FAIL}✗{FAIL:#} {}", a.assertion);
        }
        for d in &a.details {
            for line in d.lines() {
                println!("      {line}");
            }
        }
    }
    for w in &r.warnings {
        println!("    {WARN}warning:{WARN:#} {w}");
    }
    if verbose {
        for n in &r.notes {
            println!("    {DIM}note: {n}{DIM:#}");
        }
        if r.outcome != Outcome::Passed && !r.collector_logs.is_empty() {
            println!("    Collector output:");
            for line in &r.collector_logs {
                println!("      {DIM}| {line}{DIM:#}");
            }
        }
    }
    if r.outcome != Outcome::Passed {
        println!();
    }
}

pub fn summary(passed: usize, failed: usize, errors: usize, leading_blank: bool, seconds: f64) {
    let lead = if leading_blank { "\n" } else { "" };
    let passed_s = if passed > 0 {
        format!("{PASS}{passed} passed{PASS:#}")
    } else {
        format!("{passed} passed")
    };
    let failed_s = if failed > 0 {
        format!("{FAIL}{failed} failed{FAIL:#}")
    } else {
        format!("{failed} failed")
    };
    let errors_s = if errors > 0 {
        format!(", {ERROR}{errors} error(s){ERROR:#}")
    } else {
        String::new()
    };
    println!("{lead}{passed_s}, {failed_s}{errors_s}\n\nTime: {seconds:.2}s");
}

pub fn error(message: &str) {
    anstream::eprintln!("{FAIL}error:{FAIL:#} {message}");
}

pub fn display_path(p: &Path) -> String {
    let cwd = std::env::current_dir().ok();
    cwd.and_then(|c| p.strip_prefix(c).ok().map(|r| r.display().to_string()))
        .unwrap_or_else(|| p.display().to_string())
}
