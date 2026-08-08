# Auto-Split by Size

When a live recording grows past a size limit, datavis finalises the current
`.mcap` file and continues into a new one, so no single recording file grows
without bound. The parts replay back as one stitched timeline automatically.

## Configuration

Add a `[recording]` section to `config.toml`:

```toml
[recording]
# Auto-split live recordings once the .mcap on disk reaches this many MB.
# Parts are named recording_<secs>_000.mcap, _001, ... and replay stitched
# together. Omit the section or set 0 to keep a single file.
max_file_mb = 512
```

- `max_file_mb` — roll over when the on-disk `.mcap` reaches this many megabytes
  (post-compression). Omit the section, or set `0`, to keep the single-file
  behaviour.

## File naming

With a limit set, parts of one session share the session start timestamp and are
numbered sequentially:

```
recording_1785959160_000.mcap
recording_1785959160_001.mcap
recording_1785959160_002.mcap
```

With **no** limit (section omitted or `max_file_mb = 0`) the filename stays the
plain `recording_<secs>.mcap` — byte-for-byte the previous behaviour, unchanged.

## How rollover works

- Split is by **on-disk file size**, measured after the recorder's flush — it
  matches the "keep files under X" intuition and accounts for compression.
- Rollover is **approximate**: it fires at the first size check past the limit,
  so a part may overshoot by up to roughly one chunk plus a moment of data.
  Acceptable for "keep files under ~X".
- On rollover the recorder finalises the current part (writing its summary and
  chunk index, so each part is self-contained), then opens the next part and
  continues the same message stream into it. No samples are lost or duplicated
  at the boundary.
- Each new part re-registers its channels, so every part is independently
  replayable and carries its own embedded schemas.

## Replaying split recordings

Each part is a complete, self-describing MCAP file. To replay a split session,
open all its parts together in the file dialog — the loader stitches them onto
one timeline, order-independently. Nothing about playback changes.

> Auto-expanding a session's sibling parts from a single pick is a documented
> future nicety; today you multi-select the parts.

## What it does not change

The message write path, the one-second flush cadence, sequence numbering, and
the record queue are all unchanged. Playback — stitching, the lazy loader, and
every panel — is untouched. With no limit configured, behaviour is exactly as
before this feature existed.
