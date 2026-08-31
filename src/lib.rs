// SPDX-License-Identifier: GPL-3.0-or-later
//! Echo VRCE Installer.
//!
//! Split into a library and a thin binary so the engines can be tested without building a
//! window: `cargo test` exercises everything under [`engine`], none of which needs a
//! display, a VM or a headset.

pub mod app;
pub mod channel;
pub mod cli;
pub mod config;
pub mod dependencies;
pub mod endpoints;
pub mod engine;
pub mod flows;
pub mod fmt;
pub mod icons;
pub mod log;
pub mod logo;
pub mod mark;
pub mod os;
pub mod theme;
pub mod tools_screen;
pub mod update_notice;
pub mod widgets;
