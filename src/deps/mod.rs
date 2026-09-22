//! GitHub-hosted dependency support: `lait deps` manages single workflow,
//! agent, and skill files published in GitHub repositories. A hand-editable
//! manifest (`lait.deps.yml`, see [`manifest`]) declares what to fetch; a
//! generated lock file (`lait.lock`, see [`lock`]) records the commit SHA
//! and payload SHA-256 each declaration last resolved to; and the fetched
//! bytes are materialized under `.lait/deps/` so the ordinary config
//! registries (`workflows:`/`agents:`/`skills:`) can reach them by name —
//! `config::load` merges [`ops::load_registry_entries`]'s output into its
//! own maps.
//!
//! Split by role rather than by command: [`spec`] is the accepted source
//! spellings and the workflow/agent/skill classification, [`manifest`] and
//! [`lock`] are the two on-disk files, [`github`] is the REST client that
//! turns a ref into a commit and bytes, and [`ops`] is the command
//! implementations plus the registry merge. `ops` is the only piece the
//! rest of the crate needs; the others stay module-private so the
//! manifest/lock wire formats have exactly one owner each.

mod github;
mod lock;
mod manifest;
mod ops;
mod spec;
#[cfg(test)]
mod tests;

/// `schema::tests` cross-checks `schemas/deps.json` against the real
/// manifest parser; nothing outside tests needs the type itself.
#[cfg(test)]
pub(crate) use manifest::DepsManifest;
pub(crate) use ops::{
    RegistryEntries, load_registry_entries, load_registry_entries_cancellable, run_add,
    run_install, run_list, run_remove, run_update, run_verify,
};
pub(crate) use spec::DepKind;
