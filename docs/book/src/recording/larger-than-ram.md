# Larger-Than-RAM Playback

datavis plays back recordings that do not fit in memory. Rather than loading a
whole MCAP file into RAM, it memory-maps each file and decodes chunks on demand,
so even the compressed bytes need not fit in RAM.

## Why

A long or high-rate recording can easily exceed available memory. Eagerly
retaining every decoded sample would make such files unopenable. The lazy loader
removes that ceiling: memory use tracks what you are *looking at*, not the file
size.

## How it works

- **Always lazy, one path.** Every load memory-maps the file (via `memmap2`),
  builds an envelope of what is where, and decodes chunks only as panels request
  them. There is no separate small-file path — one implementation for all sizes.
- **On-demand decode.** When a panel asks for a window, the store decodes just
  the chunk(s) covering that range and serves the slice. Scrub elsewhere and
  different chunks are paged in; the OS pages the compressed bytes as needed.
- **Single-level envelope.** For wide zoom-outs the store serves a decimated
  min/max envelope rather than every sample. There is one envelope level, not a
  multi-resolution pyramid.
- **Stitched across parts.** Loading multiple files (a
  [split](auto-split.md) session) builds one combined timeline over all of them.

## Trade-offs and limits

These are deliberate, documented v1 limitations:

- **Blocky mid-zoom fidelity.** In the narrow band that is too wide for
  full-detail decode yet thin on envelope buckets, the trace can look blocky. A
  multi-level level-of-detail pyramid is future work.
- **Text/log channels stay fully in RAM.** Logs are low-volume, so this is
  cheap, but it is the one component that remains `O(file)` in memory. Spilling
  them to on-demand decode is future work.
- **High-rate state graphs at full-file zoom show approximate bands.** A
  high-rate `int` channel viewed at near-whole-file zoom uses the envelope, so
  its state bands are approximate. Low-rate state channels touch few chunks and
  stay on the exact detail path, so they are unaffected.

## Error handling

- A missing or short summary falls back to a per-file message scan for bounds
  and a single whole-file span (that file loses chunk-level laziness but still
  loads).
- An mmap failure propagates with the file path in the error, just like a plain
  read failure would.

## Future work

Multi-level LOD pyramids for smooth fidelity at every zoom, lazy (spill-to-
decode) text channels, and prefetch of neighbouring chunks to hide scrub-time
decode latency are all noted for later.
