//! Safe wrappers around the small set of libc process primitives the Node and
//! the merge CLI need that the pinned nix 0.28 does not expose on every
//! supported platform.
//!
//! r20 item 1 / coordinator ruling R8(c): the non-reaping exit observer MUST
//! compile and work on BOTH Linux and macOS. nix 0.28 gates `waitid`/`Id` off
//! Apple targets, so they do not link there; `libc::waitid` with
//! `WEXITED | WNOHANG | WNOWAIT` is in POSIX.1-2008 and is supported by glibc,
//! musl and Apple libSystem. This crate is the single place that makes the
//! `unsafe` calls; every wrapper keeps a strict "observation vs inference"
//! discipline so callers can never signal a process group they did not observe
//! exit.
#![deny(missing_docs)]

#[cfg(unix)]
mod imp;

#[cfg(unix)]
pub use imp::ObservedChildExit;

#[cfg(unix)]
pub use imp::observe_exit_without_reaping;
