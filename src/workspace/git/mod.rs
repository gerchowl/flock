mod config;
#[cfg(test)]
mod config_tests;
mod discovery;
mod status;
#[cfg(test)]
mod test_support;

pub use self::{
    discovery::{
        derive_label_from_cwd, git_branch, git_space_metadata, project_key_for_common_dir,
        GitSpaceMetadata,
    },
    status::{git_status_cache_key, git_status_snapshot_for_cwd, GitStatusCacheEntry},
};

pub(crate) use self::discovery::{
    canonicalize_best_effort_path, invalidate_path_canonicalization, path_canonicalization_epoch,
};

#[cfg(test)]
pub(super) use self::status::git_ahead_behind;

#[cfg(test)]
pub(crate) use self::test_support::temp_test_dir as test_support_temp_dir;
