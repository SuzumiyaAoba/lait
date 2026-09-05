use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::Result;

/// Lists a path-backed registry in stable name order and keeps malformed
/// entries visible as warnings. The loader returns the display path
/// separately so registries whose value names a directory (skills) can show
/// the resolved file while the other registries display their configured
/// path. Loading remains lazy and domain-specific; this module owns only the
/// shared ordering and output contract.
pub(crate) fn list_path_registry<F>(
    registry_name: &str,
    entries: &HashMap<String, PathBuf>,
    mut load: F,
) -> Result<()>
where
    F: FnMut(&str, &Path) -> (PathBuf, Result<Option<String>>),
{
    if entries.is_empty() {
        println!(
            "no {registry_name} defined in {}; add a '{registry_name}:' entry to define one",
            crate::config::CONFIG_FILE_NAME
        );
        return Ok(());
    }

    let mut names: Vec<&String> = entries.keys().collect();
    names.sort_unstable();
    for name in names {
        let (path, loaded) = load(name, &entries[name]);
        print_entry(name, &path, loaded);
    }
    Ok(())
}

fn print_entry(name: &str, path: &Path, loaded: Result<Option<String>>) {
    match loaded {
        Ok(Some(description)) => println!("{name}  ({}): {description}", path.display()),
        Ok(None) => println!("{name}  ({})", path.display()),
        Err(error) => {
            println!("{name}  ({})", path.display());
            println!("  warning: {error:#}");
        }
    }
}
