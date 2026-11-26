//! The merged compaction brief and Claude Code hook handling.

pub mod brief;
pub mod hook;
mod run;

pub use brief::{assemble, compose_brief, compose_brief_with, Section};
pub use hook::run_hook;
pub use run::{run_command, CmdOut};
