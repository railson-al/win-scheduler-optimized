//! ScriptRunner's graphical front end: just another client of the daemon (plan, leva 3).
//!
//! Everything that is not drawing lives in plain modules that need no window to be tested:
//!
//! * [`backend`]: the thread that talks to the daemon, so the UI never waits for IPC (D16, D17).
//! * [`fmt`]: durations, local dates, schedule summaries, status labels (D18, D19).
//! * [`i18n`]: the one table of user-visible text (pt-BR for now).

pub mod app;
pub mod backend;
pub mod fmt;
pub mod form;
pub mod i18n;
pub mod state;
pub mod views;
