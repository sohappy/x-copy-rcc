// SPDX-License-Identifier: GPL-3.0-or-later

//! Core of HappyGreaseFloppy.
//!
//! This crate has no GUI dependency. It knows how to:
//! * describe every Greaseweazle `gw` action and turn it into command-line arguments ([`gw`]);
//! * validate user input before touching a disk ([`gw::Action::validate`]);
//! * run `gw` in the background with live output and cancellation ([`runner`]);
//! * understand `gw` output ([`parse`]) and turn it into a per-track disk map ([`diskmap`]);
//! * list disk formats and image types ([`formats`]);
//! * inspect image files, including a read-only AmigaDOS explorer ([`image`]).

pub mod acorndfs;
pub mod amigados;
pub mod apple2;
pub mod cbmdos;
pub mod compat;
pub mod diskmap;
pub mod fat12;
pub mod formats;
pub mod gw;
pub mod health;
pub mod hints;
pub mod image;
pub mod imageopts;
pub mod locate;
pub mod naming;
pub mod parse;
pub mod ports;
pub mod runner;
pub mod trackset;

pub use diskmap::{DiskMap, MapMode, TrackInfo, TrackState};
pub use gw::{Action, ActionKind, Drive, GwConfig};
pub use parse::{Output, OutputParser};
pub use runner::{Job, JobEvent};
pub use trackset::TrackSet;

/// Crate version, shown in the GUI title bar.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
