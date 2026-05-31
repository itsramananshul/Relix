"""Tests for the PART 7 ``SSEParser`` state machine.

The parser is the framing layer the Relix Python SDK uses to translate
the bridge's `event: chunk\\ndata: ...\\n\\n` wire shape into
``StreamChunk`` frames. These tests target the parser directly so a
framing regression fails before any of the higher-level chat_stream
tests do.
"""

from __future__ import annotations

from relix.client import SSEParser


def test_sse_parser_assembles_events_split_across_two_chunks() -> None:
    """One event whose bytes are delivered in two ``feed`` calls must
    still surface as a single event on the second call."""
    parser = SSEParser()
    # Split mid-payload: first half carries `event: chunk\ndata: hel`,
    # second half carries `lo\n\n`. The parser must buffer the partial
    # bytes between the two feeds.
    events1 = parser.feed("event: chunk\ndata: hel")
    assert events1 == [], "no complete event in the first half"
    events2 = parser.feed("lo\n\n")
    assert len(events2) == 1
    assert events2[0]["event"] == "chunk"
    assert events2[0]["data"] == "hello"


def test_sse_parser_handles_three_consecutive_events_in_one_chunk() -> None:
    """Three events delivered as a single bytestring must produce
    three entries from a single ``feed`` call."""
    parser = SSEParser()
    body = (
        "event: chunk\ndata: foo\n\n"
        "event: chunk\ndata: bar\n\n"
        "event: chunk\ndata: baz\n\n"
    )
    events = parser.feed(body)
    assert len(events) == 3
    assert [e["data"] for e in events] == ["foo", "bar", "baz"]
    assert {e["event"] for e in events} == {"chunk"}


def test_sse_parser_holds_partial_tail_for_next_feed() -> None:
    """A trailing partial event must remain in the buffer until the
    next ``feed`` completes it. ``flush`` then drains any leftover."""
    parser = SSEParser()
    events = parser.feed("event: chunk\ndata: a\n\nevent: chunk\ndata: tail")
    assert len(events) == 1
    assert events[0]["data"] == "a"
    # The tail is held until either another `\n\n` arrives via feed
    # or the caller flushes the stream end.
    flushed = parser.flush()
    assert len(flushed) == 1
    assert flushed[0]["data"] == "tail"


def test_sse_parser_ignores_comment_and_field_less_lines() -> None:
    """Lines starting with `:` are SSE keep-alive comments. Lines
    without a `:` are not valid SSE fields. Both must be silently
    skipped without breaking the event that contains them."""
    parser = SSEParser()
    events = parser.feed(": ping\nevent: chunk\nnot-a-field\ndata: ok\n\n")
    assert len(events) == 1
    assert events[0]["event"] == "chunk"
    assert events[0]["data"] == "ok"


def test_sse_parser_returns_empty_when_only_partial_data_buffered() -> None:
    """An incomplete first frame must yield zero events; the parser
    must not surface a half-frame."""
    parser = SSEParser()
    events = parser.feed("event: chunk\ndata: incompl")
    assert events == []
