use core::fmt;

use tracing::{Event, field::Visit};
use tracing_subscriber::{
    fmt::{
        FmtContext,
        format::{FormatEvent, FormatFields, Writer},
    },
    registry::LookupSpan,
};

/// Simple formatter that prints the message field without applying level-based
/// coloring. ANSI codes in the message are kept unless `strip_ansi` is set.
#[derive(Default)]
pub struct PlainFormatter {
    display_level: bool,
    display_timestamp: bool,
    strip_ansi: bool,
}

impl PlainFormatter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_level(mut self, display: bool) -> Self {
        self.display_level = display;
        self
    }

    #[must_use]
    pub fn with_timestamp(mut self, display: bool) -> Self {
        self.display_timestamp = display;
        self
    }

    /// Drop ANSI escapes, for a writer that is not a terminal.
    #[must_use]
    pub fn with_strip_ansi(mut self, strip: bool) -> Self {
        self.strip_ansi = strip;
        self
    }
}

impl<S, N> FormatEvent<S, N> for PlainFormatter
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut visitor = MessageVisitor { message: None };
        event.record(&mut visitor);
        let message = visitor.message.ok_or(fmt::Error)?;

        // Optionally write timestamp
        if self.display_timestamp {
            let now = jiff::Timestamp::now();
            write!(writer, "{now} ")?;
        }

        // Optionally write level
        if self.display_level {
            let metadata = event.metadata();
            write!(writer, "{:5} ", metadata.level())?;
        }

        if self.strip_ansi {
            writeln!(writer, "{}", strip_ansi::strip_str(&message))
        } else {
            writeln!(writer, "{message}")
        }
    }
}

/// A visitor to capture only the `message` field.
struct MessageVisitor {
    message: Option<String>,
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use owo_colors::OwoColorize as _;
    use tracing_subscriber::fmt::SubscriberBuilder;

    use super::PlainFormatter;
    use crate::output::BufferedOutput;

    fn format(formatter: PlainFormatter) -> String {
        let buffer = Arc::new(BufferedOutput::new());
        let subscriber = SubscriberBuilder::default()
            .with_writer(Arc::clone(&buffer))
            .event_format(formatter)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("{} {}", "plain", "dim".dimmed());
        });
        buffer.get_buffer()
    }

    #[test]
    fn strip_ansi_drops_escapes() {
        assert_eq!(
            format(PlainFormatter::new().with_strip_ansi(true)),
            "plain dim\n"
        );
    }

    #[test]
    fn default_keeps_escapes() {
        assert_eq!(
            format(PlainFormatter::new()),
            format!("plain {}\n", "dim".dimmed())
        );
    }
}
