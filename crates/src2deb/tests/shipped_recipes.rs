//! Every recipe this repository ships has to load.
//!
//! [`Recipe::load`] validates as well as parses, so a check tightened in the
//! library can reject a recipe that used to be fine. The recipes are the
//! project's own worked examples and the thing a first run builds, so that has
//! to fail here rather than in front of whoever followed the quick start.

use std::path::{Path, PathBuf};

use src2deb::Recipe;

/// The `recipes/` directory at the repository root, found from this crate's own
/// manifest so the test does not depend on the working directory.
fn recipes_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../recipes")
        .canonicalize()
        .expect("the repository ships a recipes directory")
}

#[test]
fn every_shipped_recipe_loads_and_validates() {
    let mut loaded = 0;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(recipes_dir())
        .expect("reading the recipes directory")
        .map(|entry| entry.expect("reading a recipe directory entry").path())
        .collect();
    // Sorted, so a failure names the same recipe on every host.
    entries.sort();

    for dir in entries {
        if !dir.join("recipe.toml").is_file() {
            continue;
        }
        let recipe = Recipe::load(&dir)
            .unwrap_or_else(|err| panic!("{} does not load: {err}", dir.display()));
        // A recipe that parsed but declared nothing would pass vacuously.
        assert!(
            !recipe.components.is_empty(),
            "{} declares no components",
            dir.display()
        );
        loaded += 1;
    }

    // The directory going missing, or being renamed, must not read as success.
    assert!(loaded >= 3, "only {loaded} recipe(s) were found to load");
}
