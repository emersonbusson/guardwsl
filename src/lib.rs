//! GuardWSL v1: disk protection, safe cleanup, and one heavy build at a time.

pub mod admission;
pub mod build_gate;
pub mod cleanup;
pub mod config;
pub mod fsutil;
pub mod history;
pub mod host;
pub mod maintenance_lock;
pub mod repository;
