//! ScrollingTilingManager — shared library crate
//!
//! This crate contains all internal modules shared by the three binaries:
//! `stmd` (daemon), `stm` (CLI client), and `stm-watchdog` (recovery helper).

pub mod common;
pub mod config;
pub mod layout;
