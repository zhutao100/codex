use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputEnd {
    Eof,
    ReadError,
    Cutoff,
}

/// An optional reader-side tap. Implementations must not block the reader on
/// a consumer, filesystem writes, or the preview broadcast channel.
pub trait OutputSink: Send + Sync {
    fn observe(&self, stream: OutputStream, bytes: &[u8]);
    fn finish(&self, stream: OutputStream, end: OutputEnd);
}

pub struct OutputReader {
    sink: Option<Arc<dyn OutputSink>>,
    stream: OutputStream,
}

impl OutputReader {
    pub fn new(sink: Option<Arc<dyn OutputSink>>, stream: OutputStream) -> Self {
        Self { sink, stream }
    }

    pub fn observe(&self, bytes: &[u8]) {
        if let Some(sink) = &self.sink {
            sink.observe(self.stream, bytes);
        }
    }

    pub fn finish(mut self, end: OutputEnd) {
        self.finish_once(end);
    }

    fn finish_once(&mut self, end: OutputEnd) {
        if let Some(sink) = self.sink.take() {
            sink.finish(self.stream, end);
        }
    }
}

impl Drop for OutputReader {
    fn drop(&mut self) {
        self.finish_once(OutputEnd::Cutoff);
    }
}
