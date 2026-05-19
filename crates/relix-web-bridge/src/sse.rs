//! Bridge-level SSE helpers.
//!
//! ## Honest scope (SIMP-019)
//!
//! The alpha SOL VM and `RemoteCallDispatcher` are synchronous (SIMP-001 +
//! SIMP-014): the chat flow completes in full before the bridge sees the
//! final reply. "Streaming" here therefore means slicing the *already-
//! materialised* reply into SSE chunks at the HTTP boundary, NOT true
//! provider-native token streaming.
//!
//! This gives Open WebUI and other OpenAI-compatible clients a familiar UX
//! today, while remaining honest about what is happening underneath. True
//! per-token streaming arrives with the durable yield model (RELIX-2 +
//! RELIX-7) at Gate 2.

use std::convert::Infallible;
use std::time::Duration;

use async_stream::stream;
use axum::response::sse::Event;
use futures::Stream;

/// Slice a UTF-8 string into roughly `chunk_bytes`-sized chunks at character
/// boundaries. Never splits a multi-byte codepoint.
pub fn split_utf8_into_chunks(s: &str, chunk_bytes: usize) -> Vec<String> {
    let chunk_bytes = chunk_bytes.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        if current.len() + ch.len_utf8() > chunk_bytes && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Build an SSE stream that emits one `event: chunk` per slice plus a final
/// `event: done` event carrying the supplied trailing JSON payload.
///
/// Each chunk is delivered as plain text in the SSE `data:` field. The
/// `done` event's `data` is the caller-supplied JSON (e.g. `{"flow_id":"…",
/// "trace_id":"…"}`). Errors bubble as `event: error` and end the stream.
pub fn build_chunked_sse(
    text: String,
    chunk_bytes: usize,
    chunk_delay: Duration,
    done_payload: String,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    stream! {
        for chunk in split_utf8_into_chunks(&text, chunk_bytes) {
            yield Ok(Event::default().event("chunk").data(chunk));
            if !chunk_delay.is_zero() {
                tokio::time::sleep(chunk_delay).await;
            }
        }
        yield Ok(Event::default().event("done").data(done_payload));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_utf8_basic_ascii() {
        let chunks = split_utf8_into_chunks("abcdefg", 3);
        assert_eq!(chunks, vec!["abc", "def", "g"]);
    }

    #[test]
    fn split_utf8_never_splits_codepoint() {
        // "日本語" — each char is 3 bytes in UTF-8.
        let chunks = split_utf8_into_chunks("日本語", 4);
        for c in &chunks {
            assert!(c.is_char_boundary(0));
            assert!(c.is_char_boundary(c.len()));
        }
        assert_eq!(chunks.concat(), "日本語");
    }

    #[test]
    fn split_utf8_empty_input_yields_nothing() {
        let chunks = split_utf8_into_chunks("", 8);
        assert!(chunks.is_empty());
    }

    #[test]
    fn split_utf8_zero_chunk_bytes_treated_as_one() {
        let chunks = split_utf8_into_chunks("ab", 0);
        assert_eq!(chunks, vec!["a", "b"]);
    }
}
