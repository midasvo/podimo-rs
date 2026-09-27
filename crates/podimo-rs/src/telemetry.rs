//! Tracing setup. Default line format is `LEVEL | YYYY-MM-DDThh:mm:ssZ | message`.
//! Set `PODIMO_LOG_JSON=true` to switch to structured JSON.

use std::env;

use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::EnvFilter;

use crate::util::parse_bool_loose;

struct PodimoTimer;

impl FormatTime for PodimoTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"))
    }
}

pub fn init(debug: bool) {
    let filter = match env::var("RUST_LOG") {
        Ok(val) if !val.trim().is_empty() => {
            let filter = EnvFilter::new(&val);
            if !val.contains("hyper") && !val.contains("reqwest") && !val.contains("h2") {
                filter
                    .add_directive("hyper=warn".parse().expect("valid directive"))
                    .add_directive("hyper_util=warn".parse().expect("valid directive"))
                    .add_directive("reqwest=warn".parse().expect("valid directive"))
                    .add_directive("h2=warn".parse().expect("valid directive"))
                    .add_directive("rustls=warn".parse().expect("valid directive"))
            } else {
                filter
            }
        }
        _ => {
            let default_directives = if debug {
                "podimo=debug,tower_http=debug,hyper=warn,hyper_util=warn,reqwest=warn,h2=warn,rustls=warn,info"
            } else {
                "podimo=info,tower_http=info,hyper=warn,hyper_util=warn,reqwest=warn,h2=warn,rustls=warn,warn"
            };
            EnvFilter::new(default_directives)
        }
    };

    let json = env::var("PODIMO_LOG_JSON")
        .ok()
        .map(|v| parse_bool_loose(&v))
        .unwrap_or(false);

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);

    if json {
        let _ = builder.json().try_init();
    } else {
        let _ = builder
            .with_timer(PodimoTimer)
            .with_level(true)
            .event_format(LineFormat)
            .try_init();
    }
}

struct LineFormat;

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for LineFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let meta = event.metadata();
        let level = meta.level().as_str();
        write!(writer, "{level} | ")?;
        PodimoTimer.format_time(&mut writer)?;
        write!(writer, " | ")?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}
