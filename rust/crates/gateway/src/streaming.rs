use sse_core::{SseDecoder, SseEvent};
use std::num::NonZeroUsize;

use crate::usage::CompletionUsage;

const MAX_EVENT_BYTES: usize = 64 * 1024;

/// Observes, but never buffers or rewrites, the downstream response stream.
/// SSE framing/UTF-8 handling belongs to the codec, not a line-splitting parser.
pub(crate) struct StreamMeter {
    decoder: SseDecoder,
    pub usage: Option<CompletionUsage>,
    pub done: bool,
    pub invalid: bool,
}

impl Default for StreamMeter {
    fn default() -> Self {
        Self {
            decoder: SseDecoder::with_limit(NonZeroUsize::new(MAX_EVENT_BYTES).unwrap()),
            usage: None,
            done: false,
            invalid: false,
        }
    }
}

impl StreamMeter {
    pub fn observe(&mut self, bytes: &[u8]) -> Result<(), String> {
        if self.invalid {
            return Err("stream metering already failed".into());
        }
        let result = self.decode(bytes);
        if result.is_err() {
            self.invalid = true;
            self.usage = None;
            self.decoder.clear();
        }
        result
    }

    fn decode(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut buffer = bytes;
        while let Some(frame) = self.decoder.next(&mut buffer) {
            let SseEvent::Message(event) = frame.map_err(|e| e.to_string())? else {
                continue;
            };
            if self.done {
                return Err("SSE event received after [DONE]".into());
            }
            if event.data.trim() == "[DONE]" {
                self.done = true;
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(&event.data)
                .map_err(|_| "invalid JSON in provider SSE event")?;
            if value.get("error").is_some_and(|error| !error.is_null()) {
                return Err("provider returned an SSE error event".into());
            }
            if let Some(value) = value.get("usage").filter(|value| !value.is_null()) {
                let usage: CompletionUsage = serde_json::from_value(value.clone())
                    .map_err(|_| "invalid usage in provider SSE event")?;
                if self
                    .usage
                    .as_ref()
                    .is_some_and(|previous| previous != &usage)
                {
                    return Err("conflicting final usage in provider SSE stream".into());
                }
                self.usage = Some(usage);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_every_utf8_and_crlf_split_and_does_not_sum_repeated_usage() {
        let data = ": heartbeat\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}],\"usage\":null}\r\n\r\ndata: {\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3}}\r\n\r\ndata: {\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n";
        for split in 0..=data.len() {
            let mut meter = StreamMeter::default();
            meter
                .observe(&data.as_bytes()[..split])
                .unwrap_or_else(|e| panic!("split {split} prefix: {e}"));
            meter
                .observe(&data.as_bytes()[split..])
                .unwrap_or_else(|e| panic!("split {split} suffix: {e}"));
            assert!(meter.done);
            assert_eq!(
                meter.usage.unwrap(),
                CompletionUsage {
                    prompt_tokens: 2,
                    completion_tokens: 3
                }
            );
        }
    }

    #[test]
    fn long_stream_keeps_only_one_event_in_memory() {
        let mut meter = StreamMeter::default();
        for _ in 0..40_000 {
            meter
                .observe(b"data: {\"choices\":[],\"usage\":null}\n\n")
                .unwrap();
        }
        assert!(meter.usage.is_none());
    }

    #[test]
    fn rejects_oversized_or_conflicting_or_error_events() {
        let mut meter = StreamMeter::default();
        let oversized = format!("data: {}", "a".repeat(MAX_EVENT_BYTES + 1));
        assert!(meter.observe(oversized.as_bytes()).is_err());
        assert!(meter.invalid);
        let mut meter = StreamMeter::default();
        meter
            .observe(b"data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n")
            .unwrap();
        assert!(
            meter
                .observe(b"data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":3}}\n\n")
                .is_err()
        );
        assert!(meter.usage.is_none());
        assert!(
            StreamMeter::default()
                .observe(b"data: {\"error\":{\"message\":\"failed\"}}\n\n")
                .is_err()
        );
    }

    #[test]
    fn eof_without_terminal_marker_is_not_success() {
        let mut meter = StreamMeter::default();
        meter.observe(b"data: {\"usage\":null}\n\n").unwrap();
        assert!(!meter.done);
        assert!(meter.usage.is_none());
    }
}
