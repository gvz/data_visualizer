use eframe::egui;

/// Minimum normalized panel size (width and height).
pub const MIN: f32 = 0.05;
/// Snap threshold in screen pixels, converted to normalized per-axis per frame.
pub const SNAP_PX: f32 = 8.0;

/// A rectangle in normalized canvas coordinates; every field is `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormRect {
    pub fx: f32,
    pub fy: f32,
    pub fw: f32,
    pub fh: f32,
}

/// One panel's placement on the canvas. Parallel to `ScreenState::panels`.
#[derive(Debug, Clone, Copy)]
pub struct PanelPlacement {
    pub rect: NormRect,
}

impl NormRect {
    pub fn from_array(a: [f32; 4]) -> Self {
        Self { fx: a[0], fy: a[1], fw: a[2], fh: a[3] }
    }
    pub fn to_array(self) -> [f32; 4] {
        [self.fx, self.fy, self.fw, self.fh]
    }
    pub fn right(self) -> f32 {
        self.fx + self.fw
    }
    pub fn bottom(self) -> f32 {
        self.fy + self.fh
    }
    fn overlaps_x(self, o: NormRect) -> bool {
        self.fx < o.right() && o.fx < self.right()
    }
    fn overlaps_y(self, o: NormRect) -> bool {
        self.fy < o.bottom() && o.fy < self.bottom()
    }
    /// Map to pixel-space against the canvas rect.
    pub fn to_pixels(self, c: egui::Rect) -> egui::Rect {
        egui::Rect::from_min_size(
            egui::pos2(c.left() + self.fx * c.width(), c.top() + self.fy * c.height()),
            egui::vec2(self.fw * c.width(), self.fh * c.height()),
        )
    }
}

/// Convert a pixel delta to a normalized delta against the canvas size.
pub fn px_delta_to_norm(d: egui::Vec2, c: egui::Rect) -> egui::Vec2 {
    egui::vec2(d.x / c.width().max(1.0), d.y / c.height().max(1.0))
}

/// Snap position: pull the left or right edge to the nearest vertical guide
/// (neighbor edges + canvas borders), and top/bottom to horizontal guides,
/// within `snap`. Keeps size fixed.
pub fn snap_move(mut r: NormRect, others: &[NormRect], snap: egui::Vec2) -> NormRect {
    let mut xs = vec![0.0f32, 1.0];
    for o in others {
        xs.push(o.fx);
        xs.push(o.right());
    }
    let (mut best, mut adj) = (snap.x, 0.0f32);
    for g in &xs {
        for edge in [r.fx, r.right()] {
            let d = g - edge;
            if d.abs() < best {
                best = d.abs();
                adj = d;
            }
        }
    }
    r.fx += adj;

    let mut ys = vec![0.0f32, 1.0];
    for o in others {
        ys.push(o.fy);
        ys.push(o.bottom());
    }
    let (mut best, mut adj) = (snap.y, 0.0f32);
    for g in &ys {
        for edge in [r.fy, r.bottom()] {
            let d = g - edge;
            if d.abs() < best {
                best = d.abs();
                adj = d;
            }
        }
    }
    r.fy += adj;
    r
}

/// Snap size: pull the right/bottom edge to a guide, or the width/height to a
/// neighbor's dimension, whichever correction is smaller and within `snap`.
/// Top-left is fixed (bottom-right grip).
pub fn snap_resize(mut r: NormRect, others: &[NormRect], snap: egui::Vec2) -> NormRect {
    let mut xs = vec![0.0f32, 1.0];
    for o in others {
        xs.push(o.fx);
        xs.push(o.right());
    }
    let (mut best, mut new_w) = (snap.x, r.fw);
    for g in &xs {
        let d = (g - r.right()).abs();
        if d < best {
            best = d;
            new_w = g - r.fx;
        }
    }
    for o in others {
        let d = (o.fw - r.fw).abs();
        if d < best {
            best = d;
            new_w = o.fw;
        }
    }
    r.fw = new_w.max(MIN);

    let mut ys = vec![0.0f32, 1.0];
    for o in others {
        ys.push(o.fy);
        ys.push(o.bottom());
    }
    let (mut best, mut new_h) = (snap.y, r.fh);
    for g in &ys {
        let d = (g - r.bottom()).abs();
        if d < best {
            best = d;
            new_h = g - r.fy;
        }
    }
    for o in others {
        let d = (o.fh - r.fh).abs();
        if d < best {
            best = d;
            new_h = o.fh;
        }
    }
    r.fh = new_h.max(MIN);
    r
}

/// Clamp a move on the X axis so the rect (size fixed) never crosses a neighbor
/// that overlaps it on Y, and stays within `[0, 1]`.
fn clamp_x(prev: NormRect, target_fx: f32, others: &[NormRect]) -> f32 {
    let fw = prev.fw;
    let dir = target_fx - prev.fx;
    let mut fx = target_fx.clamp(0.0, 1.0 - fw);
    for o in others {
        if prev.overlaps_y(*o) {
            if dir > 0.0 && prev.right() <= o.fx + f32::EPSILON {
                fx = fx.min(o.fx - fw);
            } else if dir < 0.0 && prev.fx + f32::EPSILON >= o.right() {
                fx = fx.max(o.right());
            }
        }
    }
    fx.clamp(0.0, 1.0 - fw)
}

/// Y-axis twin of [`clamp_x`].
fn clamp_y(prev: NormRect, target_fy: f32, others: &[NormRect]) -> f32 {
    let fh = prev.fh;
    let dir = target_fy - prev.fy;
    let mut fy = target_fy.clamp(0.0, 1.0 - fh);
    for o in others {
        if prev.overlaps_x(*o) {
            if dir > 0.0 && prev.bottom() <= o.fy + f32::EPSILON {
                fy = fy.min(o.fy - fh);
            } else if dir < 0.0 && prev.fy + f32::EPSILON >= o.bottom() {
                fy = fy.max(o.bottom());
            }
        }
    }
    fy.clamp(0.0, 1.0 - fh)
}

/// Block-at-contact move. Resolves X then Y independently ("slide along wall").
pub fn resolve_move(prev: NormRect, desired: NormRect, others: &[NormRect]) -> NormRect {
    let fx = clamp_x(prev, desired.fx, others);
    let stepped = NormRect { fx, ..prev };
    let fy = clamp_y(stepped, desired.fy, others);
    NormRect { fx, fy, fw: prev.fw, fh: prev.fh }
}

/// Block-at-contact resize (bottom-right grip). Growth stops at the nearest
/// neighbor edge; enforces `MIN` and canvas bounds.
pub fn resolve_resize(prev: NormRect, desired: NormRect, others: &[NormRect]) -> NormRect {
    let mut fw = desired.fw.clamp(MIN, 1.0 - prev.fx);
    let mut fh = desired.fh.clamp(MIN, 1.0 - prev.fy);
    for o in others {
        if prev.overlaps_y(*o) && prev.fx < o.fx {
            fw = fw.min((o.fx - prev.fx).max(MIN));
        }
    }
    for o in others {
        if prev.overlaps_x(*o) && prev.fy < o.fy {
            fh = fh.min((o.fy - prev.fy).max(MIN));
        }
    }
    NormRect { fx: prev.fx, fy: prev.fy, fw, fh }
}

/// Lay `n` panels into an equal, non-overlapping flow grid. Used to seed
/// geometry when a loaded layout has no `frame`s.
pub fn auto_arrange(n: usize) -> Vec<NormRect> {
    if n == 0 {
        return Vec::new();
    }
    let cols = (n as f32).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);
    let fw = 1.0 / cols as f32;
    let fh = 1.0 / rows as f32;
    (0..n)
        .map(|i| {
            let c = i % cols;
            let r = i / cols;
            NormRect { fx: c as f32 * fw, fy: r as f32 * fh, fw, fh }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;

    fn r(fx: f32, fy: f32, fw: f32, fh: f32) -> NormRect {
        NormRect { fx, fy, fw, fh }
    }

    #[test]
    fn to_pixels_maps_into_canvas() {
        let canvas = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
        let px = r(0.5, 0.0, 0.5, 1.0).to_pixels(canvas);
        assert!((px.left() - 110.0).abs() < 1e-3);
        assert!((px.top() - 20.0).abs() < 1e-3);
        assert!((px.width() - 100.0).abs() < 1e-3);
        assert!((px.height() - 100.0).abs() < 1e-3);
    }

    #[test]
    fn snap_move_aligns_edge_within_threshold() {
        // Neighbor's left edge is at 0.50. Panel's right edge at 0.49 -> snaps flush.
        let me = r(0.29, 0.0, 0.20, 0.3);
        let neighbor = r(0.50, 0.0, 0.30, 0.3);
        let snap = egui::vec2(0.03, 0.03);
        let out = snap_move(me, &[neighbor], snap);
        assert!((out.fx + out.fw - 0.50).abs() < 1e-4, "right edge should snap to 0.50");
    }

    #[test]
    fn snap_move_ignores_far_edges() {
        let me = r(0.10, 0.0, 0.20, 0.3);
        let neighbor = r(0.50, 0.0, 0.30, 0.3);
        let snap = egui::vec2(0.03, 0.03);
        let out = snap_move(me, &[neighbor], snap);
        assert_eq!(out, me);
    }

    #[test]
    fn snap_resize_matches_neighbor_width() {
        // Neighbor width 0.30; my width 0.29 -> snaps to 0.30.
        let me = r(0.0, 0.0, 0.29, 0.3);
        let neighbor = r(0.60, 0.0, 0.30, 0.3);
        let snap = egui::vec2(0.03, 0.03);
        let out = snap_resize(me, &[neighbor], snap);
        assert!((out.fw - 0.30).abs() < 1e-4);
    }

    #[test]
    fn resolve_move_blocks_flush_against_neighbor() {
        let prev = r(0.0, 0.0, 0.30, 0.30);
        let neighbor = r(0.40, 0.0, 0.30, 0.30);
        // Try to move right into the neighbor.
        let desired = r(0.30, 0.0, 0.30, 0.30);
        let out = resolve_move(prev, desired, &[neighbor]);
        // Right edge cannot cross 0.40 -> fx capped at 0.10.
        assert!((out.fx - 0.10).abs() < 1e-4, "got {}", out.fx);
        assert!(out.fx + out.fw <= 0.40 + 1e-4);
    }

    #[test]
    fn resolve_move_slides_along_perpendicular_axis() {
        // Neighbor only overlaps in Y for part of the range; moving in Y past it
        // is unobstructed because X spans differ.
        let prev = r(0.0, 0.0, 0.30, 0.30);
        let neighbor = r(0.50, 0.0, 0.30, 0.30); // different X band
        let desired = r(0.0, 0.60, 0.30, 0.30);  // pure Y move
        let out = resolve_move(prev, desired, &[neighbor]);
        assert!((out.fy - 0.60).abs() < 1e-4);
    }

    #[test]
    fn resolve_resize_blocks_and_enforces_min() {
        let prev = r(0.0, 0.0, 0.20, 0.20);
        let neighbor = r(0.50, 0.0, 0.30, 0.20);
        let desired = r(0.0, 0.0, 0.90, 0.20); // grow width past neighbor
        let out = resolve_resize(prev, desired, &[neighbor]);
        assert!(out.fx + out.fw <= 0.50 + 1e-4, "blocked at neighbor left edge");
        assert!(out.fw >= MIN - 1e-6);
    }

    #[test]
    fn auto_arrange_is_in_bounds_and_non_overlapping() {
        for n in 1..=7usize {
            let rects = auto_arrange(n);
            assert_eq!(rects.len(), n);
            for a in &rects {
                assert!(a.fx >= -1e-6 && a.fy >= -1e-6);
                assert!(a.fx + a.fw <= 1.0 + 1e-6);
                assert!(a.fy + a.fh <= 1.0 + 1e-6);
            }
            for i in 0..n {
                for j in (i + 1)..n {
                    let (a, b) = (rects[i], rects[j]);
                    let overlap = a.fx < b.fx + b.fw - 1e-4
                        && b.fx < a.fx + a.fw - 1e-4
                        && a.fy < b.fy + b.fh - 1e-4
                        && b.fy < a.fy + a.fh - 1e-4;
                    assert!(!overlap, "rects {i} and {j} overlap");
                }
            }
        }
    }
}
