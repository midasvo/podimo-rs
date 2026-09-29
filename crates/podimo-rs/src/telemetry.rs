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

/// The HTTP stack logs every pooled connection at DEBUG, which buries our
/// own messages, so these stay at WARN unless `RUST_LOG` names one of them.
const CHATTY: [&str; 5] = ["hyper", "hyper_util", "reqwest", "h2", "rustls"];

pub fn init(debug: bool) {
    let rust_log = env::var("RUST_LOG").ok();
    // Like an unset one, a `RUST_LOG` that doesn't parse gets the defaults.
    let filter = EnvFilter::try_new(directives(rust_log.as_deref(), debug))
        .unwrap_or_else(|_| EnvFilter::new(directives(None, debug)));

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

/// `RUST_LOG` if set, else our own messages at INFO (DEBUG with `DEBUG=true`)
/// and everything else at WARN (INFO), plus the [`CHATTY`] crates at WARN.
fn directives(rust_log: Option<&str>, debug: bool) -> String {
    let base = match rust_log.map(str::trim).filter(|s| !s.is_empty()) {
        Some(spec) if CHATTY.iter().any(|name| spec.contains(name)) => return spec.to_string(),
        Some(spec) => spec,
        None if debug => "podimo=debug,info",
        None => "podimo=info,warn",
    };
    let quiet: Vec<String> = CHATTY.iter().map(|name| format!("{name}=warn")).collect();
    format!("{base},{}", quiet.join(","))
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

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: &str = "hyper=warn,hyper_util=warn,reqwest=warn,h2=warn,rustls=warn";

    #[test]
    fn debug_is_for_our_own_messages() {
        assert_eq!(directives(None, false), format!("podimo=info,warn,{QUIET}"));
        assert_eq!(directives(None, true), format!("podimo=debug,info,{QUIET}"));
        assert_eq!(directives(Some(" "), true), directives(None, true));
        assert!(EnvFilter::try_new(directives(None, true)).is_ok());
    }

    #[test]
    fn rust_log_keeps_the_http_stack_quiet_unless_it_names_it() {
        assert_eq!(directives(Some("debug"), false), format!("debug,{QUIET}"));
        assert_eq!(
            directives(Some("debug,hyper=trace"), false),
            "debug,hyper=trace"
        );
    }
}
