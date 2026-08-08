# Playback

Playback opens a recorded MCAP file and replays it through the same panels you
use live. It is an **exclusive** mode: live ingest pauses while you scrub a
recording.

## Opening a recording

Open a `recording_*.mcap` file from the toolbar / file dialog. To open a
[split](auto-split.md) session, select all of its parts together — the loader
stitches them onto one timeline, order-independently.

Because recordings are [self-describing](recording.md#the-file), datavis reads
the Protobuf schemas from the file itself. You do **not** need the original
`.proto`, the `--schema` flag, or a live source to replay. Channels
reconstructed from the file become droppable in the sidebar; leaving replay
restores your live channel tree.

## Controls

Replay exposes a playback clock over the recording's duration:

- **Play / pause.**
- **Scrub** anywhere on the timeline.
- **Variable speed** from 0.1× to 10×. The clock advances position at
  `speed × wall-time` and drives which part of the file is decoded.

Panels update from the store as the position moves, exactly as they do live.

## Reading model

As the position moves, panels ask the playback store for a window of samples
(`snapshot(channel, window)`) or the latest sample at or before a time
(`latest_at`). The store answers from whichever chunk(s) cover the requested
range. This on-demand decode is what makes very large recordings playable — see
[Larger-Than-RAM Playback](larger-than-ram.md).

## Robustness

- A recording with a **gap** (from record-queue overflow) loads fine — the
  store returns whatever subset is present.
- A **decode error** inside a chunk is logged and that message skipped; the rest
  of the chunk still decodes.
- A topic re-registered with a **conflicting type** across parts is skipped
  rather than corrupting the timeline.

## Leaving replay

Close replay to return to live mode. Your pre-replay channel tree and layout are
restored.
