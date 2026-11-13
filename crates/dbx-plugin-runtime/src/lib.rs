pub use dbx_platform::process;
pub use dbx_types::models;

pub mod plugins;

/// Plugin-initiated data transfer (`host.transfer:run`), see docs/07.
///
/// The module always compiles: the `plugin-transfer` feature marks builds that
/// *advertise* the API (`DBX_BUILD_FEATURES`), it does not gate the code. Gating the
/// code as well would make every feature combination of the host a separate build.
pub mod transfer;
