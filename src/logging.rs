use tracing::subscriber::Subscriber;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

fn stdout_layer<S>() -> Box<dyn Layer<S> + Send + Sync + 'static>
where
    S: Subscriber,
    for<'a> S: tracing_subscriber::registry::LookupSpan<'a>,
{
    use tracing_subscriber::fmt::format::FmtSpan;

    tracing_subscriber::fmt::layer()
        .pretty()
        .compact()
        .with_level(true)
        .with_file(true)
        .with_line_number(true)
        .with_target(true)
        .with_span_events(FmtSpan::CLOSE)
        .boxed()
}

/// Creates a filter from the `RUST_LOG` env var with a default of `INFO` if unset.
fn env_or_default_filter() -> EnvFilter {
    EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy()
}

fn log_filter() -> anyhow::Result<EnvFilter> {
    let directives = [
        "h2=off",
        "tower_http::trace=off",
        "hyper_util::client=off",
        "tower::buffer::worker=off",
        "miden_client::sync=off",
        "miden_prover=off",
        "winter_prover=off",
        "miden_processor=off",
        "miden_client::transaction=off",
        // Per-request debug spam we never want by default — enable explicitly via
        // RUST_LOG (e.g. `RUST_LOG='info,miden_agglayer_service::service::debug=debug'`).
        // These fire once per JSON-RPC method / per raw txn so they would drown out
        // everything else if left on.
        //
        // NOTE: `miden_agglayer_service::miden_client::sync::debug` is intentionally NOT
        // silenced here. That target carries "sync succeeded at block X" — the one signal
        // an operator needs to distinguish a healthy-idle service from a hung one. Default
        // level stays INFO so it is still suppressed by default, but flipping
        // `RUST_LOG=debug` (or targeting that directive specifically) now surfaces it
        // without needing a rebuild.
        "miden_agglayer_service::service::debug=off",
        "miden_agglayer_service::service_send_raw_txn::debug=off",
        // SECURITY: clamp the alloy/reqwest HTTP transport crates to `info`. The
        // upstream `ReqwestTransport` emits a DEBUG event on every L1 RPC call that
        // includes the full `url` field, which for our Sepolia archival endpoint
        // contains the `?apiKey=<SECRET>` query string. The org logs to a SIEM, so
        // any operator who flips `RUST_LOG=debug` would leak the credential. We add
        // these as target-specific directives AFTER the env-derived filter — they
        // take precedence over a bare `RUST_LOG=debug` because target-specific
        // directives win on specificity in `tracing_subscriber::EnvFilter`,
        // regardless of insertion order.
        //
        // If you ever need raw alloy transport tracing locally, do it explicitly
        // and only with a non-production L1 endpoint:
        //   RUST_LOG='info,my_target=debug,alloy_transport_http=debug' ...
        // and accept that the URL (with any query params) will be logged.
        "alloy_transport_http=info",
        "alloy_transport=info",
        "alloy_rpc_client=info",
        "reqwest=info",
    ];
    let mut filter = env_or_default_filter();
    for directive in directives {
        filter = filter.add_directive(directive.parse()?);
    }
    Ok(filter)
}

pub fn setup_tracing() -> anyhow::Result<()> {
    let subscriber = Registry::default().with(stdout_layer().with_filter(log_filter()?));
    tracing::subscriber::set_global_default(subscriber)?;

    // Register panic hook now that tracing is initialized.
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(panic = true, "{info}");
    }));

    Ok(())
}

#[cfg(test)]
mod tests {
    //! Regression tests for the L1-RPC apiKey leak that prompted these directives.
    //!
    //! See the "SECURITY" comment in `log_filter` above. The fix relies on
    //! target-specific directives winning over a bare `RUST_LOG=debug` because
    //! `tracing_subscriber::EnvFilter` ranks directives by specificity, not by
    //! insertion order. These tests pin that behaviour so a future refactor can't
    //! silently regress the apiKey leak.
    //!
    //! Sample of the original leak (from bali pod `miden-agglayer-0`, redacted):
    //!     DEBUG alloy_transport_http: received response from server, status: 200 OK
    //!       url: https://rpc.eu-central-1.gateway.fm/v4/ethereum/archival/sepolia
    //!       ?apiKey=<REDACTED> method_names: eth_blockNumber

    use std::sync::{Arc, Mutex};

    use tracing::Subscriber;
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;

    /// Capture layer that records `(target, level)` for every event reaching it.
    #[derive(Clone, Default)]
    struct CaptureLayer {
        seen: Arc<Mutex<Vec<(String, tracing::Level)>>>,
    }

    impl<S> Layer<S> for CaptureLayer
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let meta = event.metadata();
            self.seen
                .lock()
                .unwrap()
                .push((meta.target().to_string(), *meta.level()));
        }
    }

    /// Serializes the `RUST_LOG` critical section below. Cargo runs `#[test]`
    /// fns in parallel and `RUST_LOG` is process-global, so without this lock a
    /// sibling test can `set_var`/`remove_var` between our `set_var` and
    /// `log_filter()` read — building the filter from the wrong directive and
    /// silently dropping the events we expect to capture (`seen == []`).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Build the production filter under a forced `RUST_LOG=debug` and run `f`
    /// against a `Registry` that captures every event. This mirrors what the
    /// real binary would do if an operator set `RUST_LOG=debug` on the pod.
    fn with_debug_filter<F: FnOnce(&CaptureLayer)>(f: F) {
        // Hold the lock across the whole env-var window (set → build filter →
        // restore). A poisoned lock just means a prior test panicked while
        // holding it; the env state is still ours to overwrite, so recover it.
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("RUST_LOG").ok();
        // SAFETY: setting env vars is unsafe on some platforms (Rust 1.84+).
        unsafe {
            std::env::set_var("RUST_LOG", "debug");
        }

        let filter = super::log_filter().expect("log_filter must build");
        let capture = CaptureLayer::default();
        let subscriber =
            tracing_subscriber::Registry::default().with(capture.clone().with_filter(filter));

        tracing::subscriber::with_default(subscriber, || f(&capture));

        // SAFETY: see above.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("RUST_LOG", v),
                None => std::env::remove_var("RUST_LOG"),
            }
        }
    }

    /// The leak target. No DEBUG-or-lower event from `alloy_transport_http`
    /// must ever reach a subscriber, even under `RUST_LOG=debug`.
    #[test]
    fn logging_alloy_transport_http_debug_is_dropped_even_with_rust_log_debug() {
        with_debug_filter(|capture| {
            tracing::debug!(
                target: "alloy_transport_http",
                url = "https://rpc.example/v4/ethereum/archival/sepolia?apiKey=<REDACTED>",
                "received response from server",
            );
            tracing::trace!(
                target: "alloy_transport_http",
                url = "https://rpc.example/v4/ethereum/archival/sepolia?apiKey=<REDACTED>",
                "trace event that must not leak either",
            );

            let seen = capture.seen.lock().unwrap();
            let leaked: Vec<_> = seen
                .iter()
                .filter(|(t, _)| t == "alloy_transport_http")
                .collect();
            assert!(
                leaked.is_empty(),
                "alloy_transport_http events reached subscriber: {leaked:?} — apiKey leak regression!",
            );
        });
    }

    /// The sibling transport crates use the same convention. Clamp them too.
    /// `tracing::debug!` requires the target to be a string literal (the callsite
    /// is generated at compile time), so the targets are unrolled rather than
    /// looped.
    #[test]
    fn logging_alloy_transport_siblings_clamped_to_info() {
        with_debug_filter(|capture| {
            tracing::debug!(target: "alloy_transport", "debug must be dropped");
            tracing::info!(target: "alloy_transport", "info is allowed");
            tracing::debug!(target: "alloy_rpc_client", "debug must be dropped");
            tracing::info!(target: "alloy_rpc_client", "info is allowed");
            tracing::debug!(target: "reqwest", "debug must be dropped");
            tracing::info!(target: "reqwest", "info is allowed");

            let seen = capture.seen.lock().unwrap();
            for (target, level) in seen.iter() {
                if matches!(
                    target.as_str(),
                    "alloy_transport" | "alloy_rpc_client" | "reqwest"
                ) {
                    assert!(
                        *level <= tracing::Level::INFO,
                        "{target} event at {level:?} leaked through filter",
                    );
                }
            }
        });
    }

    /// Sanity check: own-crate DEBUG events still surface under `RUST_LOG=debug`,
    /// so the clamp didn't accidentally silence our own diagnostics.
    #[test]
    fn logging_own_crate_debug_still_surfaces() {
        with_debug_filter(|capture| {
            tracing::debug!(
                target: "miden_agglayer_service::claim_watcher",
                "own-crate debug must still reach subscriber",
            );

            let seen = capture.seen.lock().unwrap();
            assert!(
                seen.iter()
                    .any(|(t, l)| t == "miden_agglayer_service::claim_watcher"
                        && *l == tracing::Level::DEBUG),
                "own-crate debug event was suppressed by clamp — directive too broad. seen={seen:?}",
            );
        });
    }
}
