//! IconForge core library.
//!
//! Deterministic icon pipeline: superellipse mask, gradient base plates,
//! glyph compositing, ICO output, iTunes (CN+US) online source, matching,
//! snapshot manifests. No UI, no Windows-only code here (that lives in
//! the Windows apply layer, M2).

pub mod composite;
pub mod gradient;
pub mod ico;
pub mod itunes;
pub mod lnk;
pub mod mask;
pub mod matching;
pub mod pipeline;
pub mod server;
pub mod snapshot;
pub mod webdav;

#[cfg(windows)]
pub mod gui;

#[cfg(windows)]
mod ui_icons;

pub mod presets;
pub mod icon_pack;
pub mod elevation;
pub mod studio;
pub mod recovery;
pub mod editing;
#[cfg(windows)]
mod shell;

pub mod maintenance;

pub mod resources;
pub mod theme_bundle;
