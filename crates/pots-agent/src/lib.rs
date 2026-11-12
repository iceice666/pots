//! Everything in Pots below the UI: the team chat's data model and its
//! storage, the config, the agents that answer in it, their container
//! sandboxes, and the [`reply`] loop that drives one agent reply (or
//! [`claude_code`], which hands it to the owner's Claude Code CLI).

pub mod agent;
pub mod claude_code;
pub mod config;
pub mod data;
pub mod markdown;
pub mod mcp;
pub mod providers;
pub mod reply;
pub mod sandbox;
pub mod store;
pub mod worker;
