//! The merged compaction brief and Claude Code hook handling.

pub mod brief;
pub mod hook;
pub mod memory;
mod run;
pub mod snapshot;
pub mod stores;

pub use brief::{assemble, compose_brief, compose_brief_with, Section};
pub use hook::run_hook;
pub use run::{adopt_orphans, run_command, set_child_env, CmdOut};
