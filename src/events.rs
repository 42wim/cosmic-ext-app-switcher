// SPDX-License-Identifier: GPL-3.0-only

//! Types shared by the resident backend, the UI, and preview capture.

use std::sync::Arc;

/// Maximum number of preview cards the grid can show at once.
pub const MAX_PREVIEWS: usize = 18;

/// A captured, downscaled window thumbnail in RGBA8.
#[derive(Clone, Debug)]
pub struct PreviewImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ToplevelEntry {
    pub app_id: String,
    pub title: String,
    /// Stable ext-foreign-toplevel identifier.
    pub identifier: String,
}

/// Identity of the output the switcher should appear on.  `name` can be
/// missing on older compositors, so the registry id is used as a fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputTarget {
    pub id: u32,
    pub name: Option<String>,
    pub logical_size: (i32, i32),
}

impl OutputTarget {
    pub fn size(&self) -> (f32, f32) {
        (self.logical_size.0 as f32, self.logical_size.1 as f32)
    }
}

#[derive(Debug, Clone)]
pub struct ShowData {
    pub generation: u64,
    pub entries: Vec<ToplevelEntry>,
    /// Cached previews keyed by toplevel identifier.
    pub previews: Vec<(String, Arc<PreviewImage>)>,
    pub reverse: bool,
    /// Maximum number of previews to show at once, when set by the CLI.
    pub preview_count: Option<usize>,
    /// Output the switcher will appear on, when known.
    pub output: Option<OutputTarget>,
}

#[derive(Debug, Clone)]
pub enum BackendEvent {
    Show(ShowData),
    /// A resident Wayland worker failed; the daemon should exit and restart.
    Fatal(String),
    Preview {
        generation: u64,
        identifier: String,
        image: Arc<PreviewImage>,
    },
}

#[derive(Debug)]
pub enum BackendCommand {
    Show { reverse: bool },
    Activate { identifier: String },
    Cancel,
    Hide,
    Quit,
}
