# Zooming & Cursors

Time-series panels — the waveform panel in particular — support a two-mode box
zoom and cursor measurements. Zoom can also be **linked** across panels so they
share one time window.

## Box zoom on the waveform

Left-drag a box on the plot to zoom. The gesture has two modes:

- **Plain drag → snap to the dominant axis.** datavis compares the box width and
  height:
  - Wider than tall → **X zoom** (time). Horizontal scroll keeps running live on
    the other axis.
  - Taller than wide → **Y zoom** (value). The time axis is left untouched.
- **Shift + drag → free box zoom.** Both the X (time) and Y (value) axes are set
  from the box.

A minimum 5-pixel travel threshold applies to the axis being set, so a tiny
drag does nothing. For a free zoom, an axis under the threshold is left
unchanged — a mostly-horizontal Shift-drag still just zooms X.

While you drag, the panel shows a live preview of the box before it is applied
on release.

## Resetting zoom

Reset a panel's zoom to return it to the live, auto-scrolling window. There is
also a global reset that clears zoom across all linked panels at once.

## Linked time zoom

Turn on **linked zoom** from the toolbar to make participating time-series
panels (waveform, state graph) share a single time window. Zoom or scrub one and
the others follow, so a set of panels stays aligned on the same span — useful
for correlating signals across panels. The linked window is applied on top of
each panel's own effective window rule.

## Cursors & measurements

The waveform panel (and other panels where it is meaningful) supports a cursor
and a selection region. Over a selection, the panel computes and displays
**min / max / mean / RMS**. Enable cursors with the panel's `cursors` layout
key.
