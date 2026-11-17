//! Process exit codes shared by every command.

/// The gate passes.
pub const PASS: u8 = 0;
/// The gate fails.
pub const FAIL: u8 = 1;
/// The check could not run: head is not green, the canary survives, or a config or git error.
pub const ERROR: u8 = 2;
