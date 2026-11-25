//! Editor core edit subsystem.

pub mod command;
pub mod history;
pub mod ids;
pub mod indent_region;
pub mod intent;
pub mod laws;
pub mod motion;
pub mod multi_cursor;
pub mod navigation;
pub mod patch;
pub mod recorded_commands;
pub mod rectangle_edit;
pub mod register_edit;
pub mod replace_all;
pub mod restore;
pub mod splice;
pub mod transaction;
pub mod workspace_edit;
