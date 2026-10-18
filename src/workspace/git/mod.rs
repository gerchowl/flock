mod config;
#[cfg(test)]
mod config_tests;
mod discovery;
mod probe;
mod status;
#[cfg(test)]
pub(crate) mod test_support;

pub use self::{
    discovery::{
        derive_label_from_cwd, git_branch, git_space_metadata, project_key_for_common_dir,
        GitSpaceMetadata,
    },
    status::{git_status_cache_key, GitStatusCacheEntry},
};

pub(crate) use self::discovery::{
    canonicalize_best_effort_path, invalidate_path_canonicalization, path_canonicalization_epoch,
};

#[cfg(test)]
pub(super) use self::status::git_ahead_behind;

#[cfg(test)]
pub(crate) use self::test_support::temp_test_dir as test_support_temp_dir;

#[cfg(test)]
mod timeout_tests;

pub(crate) use self::status::{git_status_snapshot_in_round, GitRefreshRound};
