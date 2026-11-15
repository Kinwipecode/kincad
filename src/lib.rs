#![allow(non_snake_case)]
#![recursion_limit = "256"]

pub mod area_layout;
pub mod batch;
pub mod cleanup;
pub mod purge;
pub mod command;
pub mod config;
pub mod darstellung;
pub mod entities;
pub mod i18n;
pub mod io;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod network;
pub mod par;
pub mod perf;
pub mod layout_fuse;
pub mod layout_export;

pub mod scale;
pub mod scene;
pub mod sys;
pub mod viewer;
/// Externe Referenzen anhängen (XATTACH) — GUI-frei, siehe `viewer/xref_attach.rs`.
pub mod xref_attach;
/// Bild oder PDF als Unterlage anhängen — siehe `picture_attach.rs`.
pub mod picture_attach;
