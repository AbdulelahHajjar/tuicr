//! Version checks and installed-binary updates.

mod check;
mod install;

pub use check::{UpdateCheckResult, UpdateInfo, check_for_updates};
pub use install::{InstallMethod, UpdateError, UpdateOutcome, update_installed, update_to_version};

/// Update guidance emitted by builds from the Local forge fork.
pub const LOCAL_FORGE_UPDATE_MESSAGE: &str =
    "This is the local-forge fork build; update with tuicr-fork-update";

#[cfg(test)]
mod fork_tests {
    #[test]
    fn should_keep_fork_update_message_exact() {
        assert_eq!(
            super::LOCAL_FORGE_UPDATE_MESSAGE,
            "This is the local-forge fork build; update with tuicr-fork-update"
        );
    }
}
