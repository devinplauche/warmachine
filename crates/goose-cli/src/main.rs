#![recursion_limit = "256"]

use anyhow::Result;
use goose_cli::cli::cli;

/// Enable ANSI/VT escape sequence processing on Windows Console Host.
///
/// Without this, spinners and progress bars from cliclack/indicatif render as
/// repeated new lines instead of updating in place, because Windows Console Host
/// does not process ANSI escapes by default.
#[cfg(windows)]
fn enable_windows_vt_processing() {
    // colors_supported() has the side effect of calling SetConsoleMode with
    // ENABLE_VIRTUAL_TERMINAL_PROCESSING on the underlying console handle.
    let _ = console::Term::stdout().features().colors_supported();
    let _ = console::Term::stderr().features().colors_supported();
}

async fn run() -> Result<()> {
    if let Err(e) = goose_cli::logging::setup_logging(None) {
        eprintln!("Warning: Failed to initialize logging: {}", e);
    }

    // On-prem builds ship audit entries to the configured SIEM sink in the
    // background (best-effort; the local hash-chained log is the source of
    // truth). No-op unless WARMACHINE_ONPREM_AUDIT_SINK_URL was baked in.
    // A misconfigured sink disables only the forwarder, never the binary:
    // the desktop spawns this same binary for `warmachine serve`, so this
    // covers desktop-driven sessions too.
    #[cfg(feature = "onprem")]
    if let Err(e) = warmachine::onprem::validate_audit_sink() {
        tracing::error!("audit SIEM sink misconfigured, forwarder disabled: {e:#}");
    } else {
        warmachine::onprem::spawn_audit_forwarder();
    }

    let result = cli().await;

    #[cfg(feature = "otel")]
    if warmachine::otel::otlp::is_otlp_initialized() {
        warmachine::otel::otlp::shutdown_otlp();
    }

    result
}

fn main() -> Result<()> {
    #[cfg(windows)]
    enable_windows_vt_processing();

    // On-prem builds require FIPS 140-3 validated cryptography. Install the
    // FIPS provider before any TLS config is created (reqwest picks up the
    // process-default provider for all provider HTTP clients). Fail fast if
    // the FIPS provider did not take effect — running without it would
    // silently violate the compliance posture.
    #[cfg(feature = "onprem")]
    {
        warmachine::onprem::init_fips_crypto();
        if !warmachine::onprem::is_fips_provider_active() {
            eprintln!(
                "FATAL: on-prem build requires the FIPS 140-3 validated crypto provider, but it is not active. Refusing to start."
            );
            std::process::exit(1);
        }
    }

    let handle = std::thread::Builder::new()
        .name("goose-cli-main".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Failed to build Tokio runtime");
            runtime.block_on(run())
        })
        .map_err(|e| anyhow::anyhow!("Failed to spawn goose-cli main thread: {}", e))?;

    handle
        .join()
        .map_err(|_| anyhow::anyhow!("goose-cli main thread panicked"))?
}
