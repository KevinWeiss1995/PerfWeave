//! Structured JSON logging. Every process in PerfWeave calls `init()` once
//! at startup. The `.cursorrules` forbid silent failures, so we emit
//! actionable error fields and never swallow errors; this module only sets up
//! the subscriber.
//!
//! Env: `PERFWEAVE_LOG` (takes precedence) or `RUST_LOG` controls the filter.
//! `PERFWEAVE_LOG_FORMAT=json|pretty` (default json in prod, pretty on tty).

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub fn init(component: &'static str) {
    let filter = EnvFilter::try_from_env("PERFWEAVE_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info,perfweave=debug"));

    let format_is_json = std::env::var("PERFWEAVE_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or_else(|_| !atty_stderr());

    let registry = tracing_subscriber::registry().with(filter);

    if format_is_json {
        registry
            .with(
                fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_span_list(false)
                    .with_target(true)
                    .with_file(false)
                    .with_line_number(false),
            )
            .init();
    } else {
        registry
            .with(fmt::layer().with_target(true).with_line_number(true))
            .init();
    }

    tracing::info!(
        component,
        version = crate::PRODUCT_VERSION,
        "perfweave component starting"
    );
}

fn atty_stderr() -> bool {
    // Minimal tty probe without pulling the `atty` crate.
    #[cfg(unix)]
    unsafe {
        libc_isatty(2) != 0
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
extern "C" {
    #[link_name = "isatty"]
    fn libc_isatty(fd: i32) -> i32;
}
