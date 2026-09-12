//! Native Claude print, PTY, and background driver implementations.
//!
//! M0-07 delivers the in-process [`Driver`] contract, launch [`materialize`],
//! thin [`ProviderProfile`] / [`SecretBroker`], binary pin, and a feature-gated
//! [`FakeDriver`] that replays built-in Observations.

mod binary;
mod capabilities;
pub mod claude_print;
mod driver;
mod error;
mod flags;
mod materializer;
mod process;
mod profile;
mod recipe;

#[cfg(any(test, feature = "test-stub"))]
mod fake;

pub use binary::{BinaryPin, default_command, pin_binary, resolve_binary};
pub use capabilities::{
    ADAPTER_VERSION, MatrixMark, capability_matrix, capability_set, capability_snapshot,
};
pub use driver::{CallContext, Driver, DriverAck, RunHandle};
pub use error::{DriverError, DriverResult};
pub use materializer::{
    BinarySource, LaunchOrigin, MaterializeRequest, SessionAction, materialize,
};
pub use process::current_process_identity;
pub use profile::{
    Delegation, EnvFileSecretBroker, ProviderHealth, ProviderKind, ProviderProfile, Secret,
    SecretBroker, SecretRef,
};
pub use recipe::{
    EnvAllowlistEntry, EnvAllowlistSource, FileLifetime, FileRole, LaunchAudit, LaunchRecipe,
    MaterializedFile, RecipePermission, RecipeProvider, TECH_DEBT_M0_PERM_01,
};
pub use remuda_protocol::DriverKind;

#[cfg(any(test, feature = "test-stub"))]
pub use fake::FakeDriver;
