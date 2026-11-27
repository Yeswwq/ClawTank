//! Routes the core's `tracing` output to a host-supplied callback (os.Logger on macOS).
//!
//! Installed at most once per process. Each formatted event becomes one callback call with
//! its level, so the host can map levels to its own logging categories.

use std::io::Write;
use std::sync::OnceLock;

use tracing::{Level, Metadata};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::{EnvFilter, prelude::*, reload};

pub type LogFn = Box<dyn Fn(Level, &str) + Send + Sync>;
type FilterReloader = Box<dyn Fn(&str) -> bool + Send + Sync>;

static SINK: OnceLock<LogFn> = OnceLock::new();
static FILTER_RELOADER: OnceLock<FilterReloader> = OnceLock::new();

/// Installs the global subscriber. `filter` uses `EnvFilter` syntax (e.g. `"info,ironrdp=warn"`).
/// Returns `false` if logging was already installed (by this or any other subscriber).
pub fn install(sink: LogFn, filter: &str) -> bool {
    if SINK.set(sink).is_err() {
        return false;
    }
    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let (filter_layer, filter_handle) = reload::Layer::new(filter);
    let subscriber = tracing_subscriber::registry().with(filter_layer).with(
        tracing_subscriber::fmt::layer()
            .with_writer(CallbackMakeWriter)
            .with_ansi(false)
            .without_time()
            .with_level(false),
    );
    if subscriber.try_init().is_err() {
        return false;
    }

    let reloader = Box::new(move |filter: &str| {
        EnvFilter::try_new(filter).is_ok_and(|filter| filter_handle.reload(filter).is_ok())
    });
    let _ = FILTER_RELOADER.set(reloader);
    true
}

/// Replaces the active filter. Returns `false` if parsing fails or logging is not installed.
pub fn set_filter(filter: &str) -> bool {
    FILTER_RELOADER.get().is_some_and(|reload| reload(filter))
}

struct CallbackMakeWriter;

impl<'a> MakeWriter<'a> for CallbackMakeWriter {
    type Writer = CallbackWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CallbackWriter::new(Level::INFO)
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        CallbackWriter::new(*meta.level())
    }
}

/// Buffers one formatted event and hands it to the sink when dropped.
struct CallbackWriter {
    level: Level,
    buf: Vec<u8>,
}

impl CallbackWriter {
    fn new(level: Level) -> Self {
        Self {
            level,
            buf: Vec::with_capacity(256),
        }
    }
}

impl Write for CallbackWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Dependencies sometimes log complete protocol requests. These include untrusted
/// client-reported identity fields, so suppress the redundant raw debug PDU and keep
/// ClawTank's sanitized lifecycle logging.
fn is_raw_client_connect_initial(text: &str) -> bool {
    text.starts_with("ironrdp_acceptor::connection: Received") && text.contains("ConnectInitial")
}

/// CredSSP `TsRequest` carries SPNEGO tokens, TLS binding ciphertext and client nonce
/// material. The dependency's debug logs serialize the full PDU; ClawTank's sanitized
/// connection lifecycle already records the outcome, so drop this redundant raw event.
fn is_raw_credssp_ts_request(text: &str) -> bool {
    text.starts_with("perform_credssp_step:")
        && text.contains("ironrdp_acceptor::credssp:")
        && (text.contains("TsRequest") || text.contains("ts_request"))
}

fn is_sensitive_dependency_debug(text: &str) -> bool {
    is_raw_client_connect_initial(text) || is_raw_credssp_ts_request(text)
}

impl Drop for CallbackWriter {
    fn drop(&mut self) {
        if let Some(sink) = SINK.get() {
            let text = String::from_utf8_lossy(&self.buf);
            let text = text.trim_end();
            if !text.is_empty() && !is_sensitive_dependency_debug(text) {
                sink(self.level, text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_sensitive_dependency_debug;

    #[test]
    fn suppresses_raw_connect_initial_but_not_sanitized_lifecycle_logs() {
        let raw = "ironrdp_acceptor::connection: Received ConnectInitial \
                   { client_name: \"probe\", client_build: 0, client_product_id: Some(1) }";
        assert!(is_sensitive_dependency_debug(raw));
        let minimal_raw =
            "ironrdp_acceptor::connection: Received ConnectInitial { client_name: \"probe\" }";
        assert!(is_sensitive_dependency_debug(minimal_raw));

        let raw_credssp = "perform_credssp_step: ironrdp_acceptor::credssp: Received TsRequest { \
                           version: 6, nego_tokens: Some([78, 84, 76]), client_nonce: Some([1, 2]) }";
        assert!(is_sensitive_dependency_debug(raw_credssp));
        let sent_credssp = "perform_credssp_step: ironrdp_acceptor::credssp: Send ts_request=TsRequest { \
                            version: 6, pub_key_auth: Some([3, 4]) }";
        assert!(is_sensitive_dependency_debug(sent_credssp));

        let lifecycle = "clawtank_core::server: RDP connection established; \
                         client platform is not identified by protocol metadata \
                         client_name=\"safe\" client_build=0 client_product_id=Some(1)";
        assert!(!is_sensitive_dependency_debug(lifecycle));
    }
}
