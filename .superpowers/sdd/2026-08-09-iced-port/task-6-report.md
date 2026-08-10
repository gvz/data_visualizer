# Task 6 Report — Sidebar: channel-tree view + drag source

## Resolution chosen

**Option (b)** — dedicated public render model in `channel_tree.rs`.

Added to `src/channel_tree.rs`:
```rust
pub enum RenderNode {
    Group { label: String, children: Vec<RenderNode> },
    Leaf  { label: String, full_name: String, selected: bool },
}

impl ChannelTree {
    pub fn render_nodes(&self) -> Vec<RenderNode> { ... }
    pub fn is_selected(&self, full_name: &str) -> bool { ... }
}
```

`channel_tree.rs` remains free of any iced/UI imports — all iced widgets live in `src/ui/sidebar.rs`.

## New public API summary

### `src/channel_tree.rs`
- `pub enum RenderNode` — render-ready node; leaves carry baked-in `selected: bool`
- `pub fn ChannelTree::render_nodes(&self) -> Vec<RenderNode>` — assembles depth-first render tree; no live-store values (sidebar shows only channel names)
- `pub fn ChannelTree::is_selected(&self, full_name: &str) -> bool` — convenience accessor for the private `selected` set

### `src/ui/sidebar.rs` (new file)
- `pub fn leaf_labels(tree: &ChannelTree) -> Vec<String>` — depth-first leaf `full_name` list; used by `sidebar_lists_every_channel` test
- `pub fn view(tree: &ChannelTree) -> iced::Element<'static, crate::ui::Message>` — scrollable column of group labels and leaf buttons; selected leaves prefixed with `•`; pressing a leaf emits `Message::DragChannel(full_name)`

### `src/ui/message.rs`
- Added `Message::DragChannel(String)` — fired on leaf press; payload is the channel `full_name`

### `src/ui/app.rs`
- Added `dragging: Option<String>` field — stores the in-flight drag name (cleared or used by Task 12)
- `update(Message::DragChannel(name))` — stores name in `self.dragging`; no-op beyond that until Task 12
- `view()` — replaces `container(text("Sidebar"))` placeholder with `sidebar::view(&self.channel_tree)`

## Test results

```
# --no-default-features
test result: ok. 233 passed; 0 failed; 0 ignored
# nix develop -c cargo test  (includes scripting)
test result: ok. 239 passed; 0 failed; 0 ignored
```

## Smoke result

`DISPLAY=:0 timeout 15 nix develop -c cargo run` — compiled cleanly, app launched, sidebar renders the real channel tree (empty with default config.toml). No crash during 15-second window.

## Commit

`feat(ui): channel-tree sidebar with drag source`

## What Task 12 must wire (drop side)

1. `Message::DragChannel(name)` is already emitted on press. Task 12 needs to:
   - Detect when the user releases over a pane (use `mouse_area` on pane content or intercept `PaneResized`-like events)
   - Read `self.dragging` (set here) and call the appropriate workspace method to bind that channel to the target pane
   - Clear `self.dragging` on drop or cancel
2. Multi-select drag payload: `ChannelTree::drag_payload(&self, name, &order)` (existing `pub(crate)` method) returns the full selection when `name` is selected. Task 12 should call this with a flat leaf order to get the full drag payload instead of just `self.dragging`.
3. If Task 12 needs the flat leaf order, call `sidebar::leaf_labels(&self.channel_tree)` — it returns the depth-first leaf sequence which matches `ChannelTree`'s internal ordering.
