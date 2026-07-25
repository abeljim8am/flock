//! Provider-agnostic install scripts for the remote-agent binary.
//!
//! The scripts themselves live in `zellij-utils` because both this plugin (to
//! bootstrap a host on first open) and the `flock` binary (to reinstall one
//! during `remote-upgrade`) must generate byte-identical text. Two copies would
//! drift, and a drifted installer writes to a different destination directory
//! than the one the upgrade path checks — exactly the failure the upgrade work
//! set out to remove.

#[allow(unused_imports)]
pub use zellij_utils::remote_bootstrap::{
    classify_bootstrap_failure, debug_install_script, install_script, quote_remote_script_arg,
    reinstall_script, release_tag, BootstrapFailure, RELEASE_BASE_URL,
};
