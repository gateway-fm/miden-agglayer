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
        // more verbose debug logs
        "miden_agglayer_service::service::debug=off",
        "miden_agglayer_service::miden_client::sync::debug=off",
        "miden_agglayer_service::service_send_raw_txn::debug=off",
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
