# Recording to MCAP

datavis records a live session to an [MCAP](https://mcap.dev) file — including
the Protobuf schemas it discovered — and plays it back through the same panels.
Because panels read through the `ChannelStore` seam, **replay and live look
identical**: a panel never learns which one it is showing.

## Starting and stopping

Recording is a manual toolbar Record / Stop control. It is available whenever
**any** ingest source is active; with no source (e.g. a bare demo with recording
unsupported) the toolbar shows "Recording unavailable (no ingest source)".

Every active source feeds one shared record queue, so a session mixing ZeroMQ,
MQTT, and a bridge records them all together.

## The file

A recording is written to `recording_<secs>.mcap` in the working directory,
where `<secs>` is the session start time. The file is **self-describing**:

- **ZeroMQ channels** embed the shared descriptor set compiled from `--schema`.
- **MQTT channels** embed a per-topic generated schema (a `t_ns` timestamp field
  and a typed `value`), built on the fly as topics are discovered.

Because the schema travels inside the file, a recording replays without the
original `.proto` or the live source — see [Playback](playback.md).

## How writing works

The recorder runs on its own thread, draining the record queue:

- Each message is written to its MCAP channel with its log timestamp and a
  monotonic sequence number.
- The writer flushes about once a second.
- On stop, it drains the remaining queued messages and finalises the file
  (`writer.finish()`), which writes the summary and chunk index that make the
  file lazily loadable.

## Backpressure and gaps

The record queue is bounded. If it fills — the recorder cannot keep up — new
samples are dropped rather than blocking ingest, and the gap is accounted for.
A write failure sets a `record_failed` flag surfaced in the status bar; the app
keeps running.

## Large sessions

A long session can produce a very large single file. datavis can **auto-split**
a recording into size-bounded parts that replay as one stitched timeline — see
[Auto-Split by Size](auto-split.md). And a recording larger than RAM still plays
back, because the loader is lazy — see
[Larger-Than-RAM Playback](larger-than-ram.md).
