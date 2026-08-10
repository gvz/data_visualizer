use crate::viz::PanelMessage;

/// A unique identifier for a panel pane in the workspace grid.
///
/// Introduced in Task 4 so `Message::Panel` can route panel messages.
/// The workspace that owns the mapping from `PaneId` to `Box<dyn VizPanel>`
/// is wired up in Task 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(pub u32);

#[derive(Debug, Clone)]
pub enum Message {
    /// Periodic repaint / idle-detection tick. Rate adapts via `desired_interval`.
    Tick,

    // -----------------------------------------------------------------------
    // Toggles — fully wired now (Task 4).
    // -----------------------------------------------------------------------
    /// Show/hide the channel picker sidebar.
    ToggleSidebar,
    /// Open/close the Data Source window (no modal rendered until Task 12).
    OpenDataSource,
    /// Close the Data Source window.
    CloseDataSource,

    // -----------------------------------------------------------------------
    // Stubs — handlers added in later tasks; update returns Task::none() for now.
    // -----------------------------------------------------------------------
    /// Load a layout from the default path (Task 13).
    Load,
    /// Save the current layout (Task 13).
    SaveLayout,
    /// Save layout to a user-chosen path (Task 13).
    SaveLayoutAs,
    /// Load layout from a user-chosen path (Task 13).
    LoadLayoutFrom,
    /// Start a new recording (Task 14).
    StartRecord,
    /// Stop the current recording (Task 14).
    StopRecord,
    /// Open an MCAP recording file for replay (Task 6).
    OpenRecording,
    /// Add a new panel to the workspace (Task 5).
    AddPanel,
    /// Add a new screen/tab to the workspace (Task 5).
    AddScreen,
    /// Route a panel-local message to the identified pane (Task 5).
    Panel(PaneId, PanelMessage),
    /// Close the pane identified by [`PaneId`] (Task 5).
    ClosePane(PaneId),
    /// Forward a pane_grid resize event to the workspace (Task 5).
    PaneResized(iced::widget::pane_grid::ResizeEvent),
    /// User pressed a channel leaf in the sidebar — stores the dragged name in
    /// app state. Drop-onto-pane binding is wired in Task 12.
    DragChannel(String),
    /// Mouse released over a pane — bind the dragged channel (if any) to that pane.
    DropOnPane(PaneId),
    /// Seek the replay playhead to the given nanosecond timestamp (Task 13).
    Seek(i64),
    /// Close the replay and return to live mode (Task 13).
    CloseReplay,
}
