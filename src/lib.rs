// YaSerde 0.10's derive macros emit non-local impls. Keep this narrowly
// scoped compatibility allowance until that dependency is upgraded.
#![allow(non_local_definitions)]
#![cfg_attr(test, allow(clippy::await_holding_lock))]

extern crate derive_more;
extern crate tracing;
extern crate yaserde;
extern crate yaserde_derive;

pub mod account_scope;
pub mod auth;
pub mod cache;
pub mod config;
pub mod disk_cache;
pub mod error;
pub mod hub_cache;
pub mod jobs;
pub mod logging;
pub mod models;
pub mod observability;
pub mod playback_selection;
pub mod plex_client;
pub mod policy_store;
pub mod resolution_policy;
pub mod response;
pub mod routes;
pub mod state;
pub mod transform;
pub mod url;
pub mod utils;
pub mod web_assets;
pub mod webhooks;
//pub mod proxy;
pub mod headers;
pub mod timeout;

#[cfg(test)]
mod test_helpers;
