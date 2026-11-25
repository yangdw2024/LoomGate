use bytes::Bytes;
use futures_core::Stream;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

/// A single parsed Server-Sent Event frame emitted incrementally with zero full-body buffering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// UTF-8 boundary-safe incremental SSE byte-stream parser.
/// Buffers raw bytes until newline (`0x0A`) so multi-byte UTF-8 characters (e.g., CJK)
/// split across TCP packets are never corrupted into U+FFFD.
#[derive(Debug, Default)]
pub struct SseFrameParser {
    raw_buf: Vec<u8>,
    current_event: Option<String>,
    data_lines: Vec<String>,
}

impl SseFrameParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a raw byte chunk from upstream TCP socket and immediately emit any completed SSE frames.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.raw_buf.extend_from_slice(chunk);

        let mut frames = Vec::new();
        while let Some(pos) = self.raw_buf.iter().position(|&b| b == b'\n') {
            let mut line_bytes: Vec<u8> = self.raw_buf.drain(..=pos).collect();
            line_bytes.pop(); // remove '\n'
            if line_bytes.last() == Some(&b'\r') {
                line_bytes.pop();
            }

            let line = String::from_utf8_lossy(&line_bytes);
            if line.is_empty() {
                if !self.data_lines.is_empty() {
                    frames.push(SseFrame {
                        event: self.current_event.take(),
                        data: self.data_lines.join("\n"),
                    });
                    self.data_lines.clear();
                } else {
                    self.current_event = None;
                }
                continue;
            }

            if line.starts_with(':') {
                continue;
            }

            if let Some(rest) = line.strip_prefix("event:") {
                self.current_event = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("data:") {
                let data_part = rest.strip_prefix(' ').unwrap_or(rest);
                self.data_lines.push(data_part.to_string());
            }
        }
        frames
    }

    /// Flush any trailing line/frame when the upstream stream reaches EOF.
    pub fn finish(&mut self) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        if !self.raw_buf.is_empty() {
            let mut leftover = std::mem::take(&mut self.raw_buf);
            if leftover.last() == Some(&b'\r') {
                leftover.pop();
            }
            let line = String::from_utf8_lossy(&leftover);
            if let Some(rest) = line.strip_prefix("event:") {
                self.current_event = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("data:") {
                let data_part = rest.strip_prefix(' ').unwrap_or(rest);
                self.data_lines.push(data_part.to_string());
            }
        }
        if !self.data_lines.is_empty() {
            frames.push(SseFrame {
                event: self.current_event.take(),
                data: self.data_lines.join("\n"),
            });
            self.data_lines.clear();
        }
        frames
    }
}

/// Tracks live output bytes for fallback billing if a stream aborts before receiving `usage`.
#[derive(Debug, Default)]
pub struct StreamMetrics {
    pub output_bytes: AtomicU64,
    pub completed: AtomicUsize,
}

impl StreamMetrics {
    pub fn add_output_bytes(&self, n: u64) {
        self.output_bytes.fetch_add(n, Ordering::Relaxed);
    }

    /// Conservative fast fallback token estimate (~4 UTF-8 bytes per token) when upstream
    /// disconnects before sending final usage metadata.
    pub fn fallback_output_tokens(&self) -> u64 {
        self.output_bytes.load(Ordering::Relaxed).div_ceil(4)
    }
}

/// RAII guard that guarantees immediate Cancel Propagation:
/// When the downstream HTTP client disconnects (e.g., Ctrl+C in Claude Code), Axum drops the
/// response body stream -> `CancelPropagatingStream::drop` fires -> `cancel_token.cancel()`
/// aborts the upstream reqwest task and TCP socket immediately, preventing ghost billing.
pub struct CancelPropagatingStream {
    rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
    cancel_token: CancellationToken,
    metrics: Arc<StreamMetrics>,
    channel_id: String,
    request_id: String,
}

impl CancelPropagatingStream {
    pub fn new(
        rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
        cancel_token: CancellationToken,
        metrics: Arc<StreamMetrics>,
        channel_id: String,
        request_id: String,
    ) -> Self {
        Self {
            rx,
            cancel_token,
            metrics,
            channel_id,
            request_id,
        }
    }
}

impl Stream for CancelPropagatingStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl Drop for CancelPropagatingStream {
    fn drop(&mut self) {
        if self.metrics.completed.load(Ordering::Relaxed) == 0 {
            info!(
                request_id = %self.request_id,
                channel_id = %self.channel_id,
                fallback_output_tokens = self.metrics.fallback_output_tokens(),
                "downstream client disconnected mid-stream; propagating cancel to abort upstream TCP connection"
            );
            self.cancel_token.cancel();
        } else {
            debug!(
                request_id = %self.request_id,
                channel_id = %self.channel_id,
                "stream completed cleanly"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_multibyte_utf8_cjk_without_corruption() {
        let mut parser = SseFrameParser::new();
        // "中" in UTF-8 is [0xE4, 0xB8, 0xAD]. Split right between 0xB8 and 0xAD!
        let full = "event: content_block_delta\ndata: {\"text\":\"中文边界测试\"}\n\n".as_bytes();
        let split_at = full.iter().position(|&b| b == 0xE4).unwrap() + 2;
        let f1 = parser.push_bytes(&full[..split_at]);
        assert!(f1.is_empty());

        let f2 = parser.push_bytes(&full[split_at..]);
        assert_eq!(f2.len(), 1);
        assert_eq!(f2[0].event.as_deref(), Some("content_block_delta"));
        assert_eq!(f2[0].data, "{\"text\":\"中文边界测试\"}");
        assert!(!f2[0].data.contains('\u{FFFD}'));
    }

    #[test]
    fn drop_triggers_cancel_propagation_when_incomplete() {
        let (_tx, rx) = mpsc::channel(4);
        let token = CancellationToken::new();
        let metrics = Arc::new(StreamMetrics::default());
        metrics.add_output_bytes(400);

        let stream = CancelPropagatingStream::new(
            rx,
            token.clone(),
            Arc::clone(&metrics),
            "ch-1".into(),
            "req-1".into(),
        );
        assert!(!token.is_cancelled());
        drop(stream);
        assert!(token.is_cancelled());
        assert_eq!(metrics.fallback_output_tokens(), 100);
    }
}
