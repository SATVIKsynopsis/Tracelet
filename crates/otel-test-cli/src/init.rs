use std::path::{Path, PathBuf};

use crate::{Failure, exit};

pub const FILES: &[(&str, &str)] = &[
    (
        "otel-test.yaml",
        include_str!("../templates/otel-test.yaml"),
    ),
    (
        "collector.yaml",
        include_str!("../templates/collector.yaml"),
    ),
    (
        "fixtures/login-span.json",
        include_str!("../templates/fixtures/login-span.json"),
    ),
    (
        "fixtures/login-log.json",
        include_str!("../templates/fixtures/login-log.json"),
    ),
    (
        "fixtures/healthz-span.json",
        include_str!("../templates/fixtures/healthz-span.json"),
    ),
];

pub fn init(dir: &Path, force: bool) -> Result<Vec<PathBuf>, Failure> {
    let targets: Vec<(PathBuf, &str)> = FILES
        .iter()
        .map(|(rel, body)| (dir.join(rel), *body))
        .collect();
    if !force {
        let existing: Vec<String> = targets
            .iter()
            .filter(|(p, _)| p.exists())
            .map(|(p, _)| p.display().to_string())
            .collect();
        if !existing.is_empty() {
            return Err(Failure::new(
                exit::INVALID_SPEC,
                format!(
                    "refusing to overwrite existing files (use --force):\n  - {}",
                    existing.join("\n  - ")
                ),
            ));
        }
    }
    let mut written = Vec::with_capacity(targets.len());
    for (path, body) in targets {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Failure::new(
                    exit::EXECUTION_ERROR,
                    format!("failed to create {}: {e}", parent.display()),
                )
            })?;
        }
        std::fs::write(&path, body).map_err(|e| {
            Failure::new(
                exit::EXECUTION_ERROR,
                format!("failed to write {}: {e}", path.display()),
            )
        })?;
        written.push(path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_match_example() {
        let example =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/password-redaction");
        for (rel, body) in FILES {
            let on_disk =
                std::fs::read_to_string(example.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
            assert_eq!(
                &on_disk, body,
                "templates/{rel} differs from examples/password-redaction/{rel}"
            );
        }
    }

    #[test]
    fn refuses_to_overwrite_without_force() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("collector.yaml"), "mine").unwrap();
        let err = init(dir.path(), false).expect_err("must refuse");
        assert_eq!(err.code, exit::INVALID_SPEC);
        assert!(err.message.contains("collector.yaml"), "{}", err.message);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("collector.yaml")).unwrap(),
            "mine"
        );
        assert!(
            !dir.path().join("otel-test.yaml").exists(),
            "nothing may be written"
        );

        let written = init(dir.path(), true).unwrap();
        assert_eq!(written.len(), FILES.len());
        assert_ne!(
            std::fs::read_to_string(dir.path().join("collector.yaml")).unwrap(),
            "mine"
        );
    }
}
