//! Logging (spec §17).
//!
//! Two sinks: a size-rotated file (`core.log`, info and above) and a layer
//! that forwards warn/error records to Swift as [`CoreEvent::Log`], which
//! Swift writes with `os.Logger` so unified-logging privacy stays under
//! Swift's control. `trace` is compiled out of release builds (workspace
//! `tracing` feature), and it is the only level allowed to carry mail
//! content; secrets are wrapped in `mail_domain::Redacted`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::events::{CoreEvent, EventBus, LogLevel};

pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_FILES: usize = 5;
pub const LOG_FILE: &str = "core.log";

/// The bus of the most recently created `Core`; the global subscriber can
/// only be installed once per process, so it forwards through this.
static LOG_SINK: Mutex<Option<EventBus>> = Mutex::new(None);
static INSTALLED: OnceLock<()> = OnceLock::new();

/// Install the global subscriber (first call only) and route forwarded
/// records to `bus` (every call).
pub(crate) fn init(log_dir: Option<&Path>, bus: EventBus) {
    *LOG_SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(bus);
    INSTALLED.get_or_init(|| {
        let filter = EnvFilter::try_from_env("OPENAGC_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
        let file_layer = log_dir
            .and_then(|dir| RotatingFile::open(dir).ok())
            .map(|file| tracing_subscriber::fmt::layer().with_writer(file).with_ansi(false).with_target(true));
        let _ =
            tracing_subscriber::registry().with(filter).with(file_layer).with(ForwardLayer(Sink::Global)).try_init();
    });
}

/// Where forwarded records go: the latest core's bus, or a fixed bus (tests).
enum Sink {
    Global,
    #[cfg(test)]
    Fixed(EventBus),
}

/// Forwards warn/error events to Swift.
struct ForwardLayer(Sink);

impl<S: Subscriber> Layer<S> for ForwardLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let level = match *event.metadata().level() {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            _ => return,
        };
        let mut message = MessageVisitor::default();
        event.record(&mut message);
        let bus = match &self.0 {
            Sink::Global => LOG_SINK.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            #[cfg(test)]
            Sink::Fixed(bus) => Some(bus.clone()),
        };
        if let Some(bus) = bus {
            bus.emit(CoreEvent::Log {
                level,
                target: event.metadata().target().to_owned(),
                message: scrub(&message.0),
            });
        }
    }
}

/// Renders an event as `message key=value …`.
#[derive(Default)]
struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            let rest = std::mem::take(&mut self.0);
            let _ = write!(self.0, "{value:?}");
            if !rest.is_empty() {
                let _ = write!(self.0, "{rest}");
            }
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

/// Last line of defense for diagnostics (spec §17): error text can quote
/// an address or, in the worst case, a token. Email addresses become
/// `<email>`; Google access (`ya29.…`) and refresh (`1//…`) tokens become
/// `<token>`. Secrets are also kept out by `Redacted` at the source.
pub(crate) fn scrub(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let local = |c: char| c.is_ascii_alphanumeric() || "._%+-".contains(c);
    let domain = |c: char| c.is_ascii_alphanumeric() || ".-".contains(c);
    let token = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let rest: String = chars[i..chars.len().min(i + 5)].iter().collect();
        let starts_word = i == 0 || !token(chars[i - 1]);
        if starts_word && rest.starts_with("ya29.") {
            let mut j = i + 5;
            while j < chars.len() && token(chars[j]) {
                j += 1;
            }
            out.push_str("<token>");
            i = j;
            continue;
        }
        if starts_word && rest.starts_with("1//") {
            let mut j = i + 3;
            while j < chars.len() && token(chars[j]) {
                j += 1;
            }
            if j - i >= 23 {
                out.push_str("<token>");
                i = j;
                continue;
            }
        }
        if chars[i] == '@' {
            // Back up over the local part already written.
            let mut start = i;
            while start > 0 && local(chars[start - 1]) {
                start -= 1;
            }
            let mut end = i + 1;
            while end < chars.len() && domain(chars[end]) {
                end += 1;
            }
            let domain_part: String = chars[i + 1..end].iter().collect::<String>().trim_end_matches('.').to_owned();
            if start < i && domain_part.contains('.') && !domain_part.starts_with('.') {
                let written = i - start;
                for _ in 0..written {
                    out.pop();
                }
                out.push_str("<email>");
                i += 1 + domain_part.chars().count();
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `core.log`, rotated to `core.log.1 … core.log.{MAX_FILES-1}` when it
/// would exceed [`MAX_FILE_BYTES`].
pub(crate) struct RotatingFile {
    dir: PathBuf,
    max_bytes: u64,
    max_files: usize,
    inner: Mutex<Current>,
}

struct Current {
    file: File,
    len: u64,
}

impl RotatingFile {
    pub(crate) fn open(dir: &Path) -> io::Result<Self> {
        Self::with_limits(dir, MAX_FILE_BYTES, MAX_FILES)
    }

    fn with_limits(dir: &Path, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let file = OpenOptions::new().create(true).append(true).open(dir.join(LOG_FILE))?;
        let len = file.metadata()?.len();
        Ok(Self { dir: dir.to_owned(), max_bytes, max_files, inner: Mutex::new(Current { file, len }) })
    }

    fn rotate(&self, current: &mut Current) -> io::Result<()> {
        let path = |n: usize| {
            if n == 0 { self.dir.join(LOG_FILE) } else { self.dir.join(format!("{LOG_FILE}.{n}")) }
        };
        let _ = fs::remove_file(path(self.max_files - 1));
        for n in (0..self.max_files - 1).rev() {
            let from = path(n);
            if from.exists() {
                fs::rename(&from, path(n + 1))?;
            }
        }
        current.file = OpenOptions::new().create(true).append(true).open(path(0))?;
        current.len = 0;
        Ok(())
    }
}

pub(crate) struct RotatingWriter<'a> {
    owner: &'a RotatingFile,
    current: MutexGuard<'a, Current>,
}

impl Write for RotatingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.current.len > 0 && self.current.len + buf.len() as u64 > self.owner.max_bytes {
            self.owner.rotate(&mut self.current)?;
        }
        // Each formatted record arrives in one write; scrub it whole.
        let clean = scrub(&String::from_utf8_lossy(buf));
        self.current.file.write_all(clean.as_bytes())?;
        self.current.len += clean.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.current.file.flush()
    }
}

impl<'a> MakeWriter<'a> for RotatingFile {
    type Writer = RotatingWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingWriter { owner: self, current: self.inner.lock().unwrap_or_else(|e| e.into_inner()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openagc-log-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn diagnostics_are_scrubbed_of_addresses_and_tokens() {
        assert_eq!(scrub("invalid address \"alex.rivera+x@mail.example.org\""), "invalid address \"<email>\"");
        assert_eq!(scrub("from <a@b.co>, cc c.d@e-f.io."), "from <<email>>, cc <email>.");
        assert_eq!(scrub("Bearer ya29.a0AfB_byC-xyz123 expired"), "Bearer <token> expired");
        assert_eq!(scrub("refresh 1//0gAbcdefghijklmnopqrstuvwx rejected"), "refresh <token> rejected");
        assert_eq!(scrub("retry 1//2 in 3s; me@localhost; 5 @ noon"), "retry 1//2 in 3s; me@localhost; 5 @ noon");
        assert_eq!(scrub("résumé for ünïcode@exämple.com"), "résumé for ünïcode@exämple.com", "non-ASCII left alone");
    }

    #[test]
    fn the_log_file_is_scrubbed() {
        let dir = temp_dir("scrub");
        let file = RotatingFile::with_limits(&dir, 10_000, 2).unwrap();
        {
            let mut w = file.make_writer();
            writeln!(w, "WARN outbox op failed: invalid address sam@example.org").unwrap();
        }
        let text = fs::read_to_string(dir.join("core.log")).unwrap();
        assert!(text.contains("invalid address <email>"), "{text}");
        assert!(!text.contains("sam@"));
    }

    #[test]
    fn rotates_by_size_and_keeps_at_most_max_files() {
        let dir = temp_dir("rotate");
        let file = RotatingFile::with_limits(&dir, 100, 3).unwrap();
        for i in 0..20 {
            let mut w = file.make_writer();
            writeln!(w, "{i:02} {}", "x".repeat(40)).unwrap();
        }
        let mut names: Vec<String> =
            fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, vec!["core.log", "core.log.1", "core.log.2"]);
        for name in &names {
            let len = fs::metadata(dir.join(name)).unwrap().len();
            assert!(len <= 100, "{name} is {len} bytes");
        }
        // The newest line is in core.log.
        assert!(fs::read_to_string(dir.join("core.log")).unwrap().contains("19 "));
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<CoreEvent>>);
    impl crate::EventListener for Recorder {
        fn on_event(&self, event: CoreEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[tokio::test]
    async fn warn_and_error_are_forwarded_with_fields_and_info_is_not() {
        let rec = std::sync::Arc::new(Recorder::default());
        let bus = EventBus::start(rec.clone(), &tokio::runtime::Handle::current());
        let subscriber = tracing_subscriber::registry().with(ForwardLayer(Sink::Fixed(bus)));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("not forwarded");
            tracing::warn!(attempt = 2, "gmail rate limited");
            tracing::error!("sync failed");
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let events = rec.0.lock().unwrap().clone();
        let target = module_path!().to_owned();
        assert_eq!(
            events,
            vec![
                CoreEvent::Log {
                    level: LogLevel::Warn,
                    target: target.clone(),
                    message: "gmail rate limited attempt=2".into()
                },
                CoreEvent::Log { level: LogLevel::Error, target, message: "sync failed".into() },
            ]
        );
    }

    #[test]
    fn redacted_fields_never_reach_the_forwarded_message() {
        let mut v = MessageVisitor::default();
        let token = mail_domain::Redacted::new("ya29.secret");
        v.record_debug(&field("token"), &token);
        assert_eq!(v.0, " token=***");
    }

    fn field(name: &'static str) -> Field {
        struct Cs;
        impl tracing::Callsite for Cs {
            fn set_interest(&self, _: tracing::subscriber::Interest) {}
            fn metadata(&self) -> &tracing::Metadata<'_> {
                &META
            }
        }
        static CS: Cs = Cs;
        static META: tracing::Metadata<'static> = tracing::Metadata::new(
            "test",
            "test",
            Level::WARN,
            None,
            None,
            None,
            tracing::field::FieldSet::new(&["token"], tracing::callsite::Identifier(&CS)),
            tracing::metadata::Kind::EVENT,
        );
        META.fields().field(name).unwrap()
    }
}
