use std::path::{Path, PathBuf};

use otel_test_core::collector::Collector;
use tokio::sync::OnceCell;

static COLLECTOR: OnceCell<Collector> = OnceCell::const_new();

pub async fn collector() -> &'static Collector {
    COLLECTOR
        .get_or_init(|| async {
            let (path, source) = otel_test_core::binary::resolve(None, false, |r| {
                eprintln!("downloading {} for integration tests", r.url);
            })
            .await
            .expect("a Collector binary is required for integration tests (set OTEL_TEST_COLLECTOR_BIN)");
            eprintln!("using Collector at {} ({source:?})", path.display());
            Collector::open(&path).await.expect("Collector binary is usable")
        })
        .await
}

pub fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}
