//! Source-resolution behavior that a plain local checkout would get wrong.
//!
//! For a git source: a branch or default ref must advance to the fetched
//! upstream tip on a re-run, while a pinned commit must stay put. For a path
//! source: the tree on disk must be copied rather than built in place, and the
//! record it leaves must not read as a reproducible build. For a packaging
//! overlay: it must replace the packaging a source ships rather than merge with
//! it, contribute nothing outside `debian/`, and leave nothing behind once a
//! recipe stops declaring it.
//!
//! These drive real `git` against local repositories, so the ones that need it
//! are skipped when `git` is unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use src2deb::recipe::Component;
use src2deb::source::SourceResolver;
use src2deb::{Fingerprint, Source, SourceInput, SourceRole};

/// Whether `git` can be launched at all; the tests no-op when it cannot.
fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Runs `git` in `dir`, asserting success.
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("launch git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Initializes a git repository with a self-contained identity, so commits do
/// not depend on the machine's global git configuration.
fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "src2deb test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

/// Writes a marker file and a minimal `debian/control`, then commits, so the
/// checked-out tree carries a content marker the tests can read back.
fn commit_marker(upstream: &Path, marker: &str) {
    std::fs::write(upstream.join("marker"), marker).unwrap();
    std::fs::create_dir_all(upstream.join("debian")).unwrap();
    std::fs::write(upstream.join("debian/control"), CONTROL).unwrap();
    git(upstream, &["add", "-A"]);
    git(upstream, &["commit", "-q", "-m", marker]);
}

/// Writes a marker file and commits, leaving the tree without a `debian/` of
/// its own — the upstream a packaging overlay exists for.
fn commit_bare_marker(upstream: &Path, marker: &str) {
    std::fs::write(upstream.join("marker"), marker).unwrap();
    git(upstream, &["add", "-A"]);
    git(upstream, &["commit", "-q", "-m", marker]);
}

/// The commit a repository's `HEAD` points at.
fn head(dir: &Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A unique scratch directory under the test target's temp dir.
fn scratch(label: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("{label}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A component pointing at a local upstream repository.
fn component(name: &str, git: &Path, git_ref: Option<&str>) -> Component {
    Component {
        name: name.to_string(),
        source: Source {
            git: Some(git.to_string_lossy().into_owned()),
            git_ref: git_ref.map(str::to_string),
            ..Source::default()
        },
        packaging: None,
        patches: Vec::new(),
        extra_build_deps: Vec::new(),
    }
}

/// A component pointing at a tree on disk.
fn path_component(name: &str, path: &Path) -> Component {
    Component {
        name: name.to_string(),
        source: Source {
            path: Some(path.to_path_buf()),
            ..Source::default()
        },
        packaging: None,
        patches: Vec::new(),
        extra_build_deps: Vec::new(),
    }
}

/// A resolver whose relative paths are taken from `recipe_dir`.
fn resolver_in(root: &Path, recipe_dir: &Path) -> SourceResolver {
    SourceResolver::new(root, recipe_dir)
}

/// `component` with a patch series applied over it.
fn patched(mut component: Component, patches: &[&str]) -> Component {
    component.patches = patches.iter().map(PathBuf::from).collect();
    component
}

/// The file the patch tests rewrite, and its unpatched content. Written with a
/// trailing newline so an ordinary unified diff applies to it.
const PATCHABLE: &str = "patchme.txt";
const BEFORE: &str = "before\n";

/// A patch that rewrites [`PATCHABLE`] to `to`.
fn rewrite_patch(to: &str) -> String {
    format!("--- a/{PATCHABLE}\n+++ b/{PATCHABLE}\n@@ -1 +1 @@\n-before\n+{to}\n")
}

/// A patch that adds a file the tree does not have, which is the case a re-run
/// has to discard before it can apply the series again.
const ADDS_A_FILE: &str = "diff --git a/added.txt b/added.txt\n\
     new file mode 100644\n\
     --- /dev/null\n\
     +++ b/added.txt\n\
     @@ -0,0 +1 @@\n\
     +added by a patch\n";

/// Writes a patch file into `dir` under `name`.
fn write_patch(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

/// The content of [`PATCHABLE`] in a resolved tree.
fn patched_content(tree: &Path) -> String {
    std::fs::read_to_string(tree.join(PATCHABLE)).unwrap()
}

/// A minimal source tree on disk: a `debian/control` and a marker file.
fn write_tree(dir: &Path, marker: &str) {
    std::fs::create_dir_all(dir.join("debian")).unwrap();
    std::fs::write(dir.join("debian/control"), CONTROL).unwrap();
    std::fs::write(dir.join("marker"), marker).unwrap();
}

/// A source tree carrying no `debian/` of its own, which is the case a
/// packaging overlay exists for.
fn write_bare_tree(dir: &Path, marker: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("marker"), marker).unwrap();
}

/// The marker content in a resolved tree.
fn resolved_marker(tree: &Path) -> String {
    std::fs::read_to_string(tree.join("marker")).unwrap()
}

/// The `debian/control` every tree in these tests carries, wherever it came
/// from.
const CONTROL: &str = "Source: pkg\n\nPackage: pkg\nArchitecture: any\n";

/// A packaging tree: a `debian/` holding a control file and a marker the tests
/// read back, and — outside `debian/` — a file of the same name the component's
/// own source uses, which an overlay must not carry across.
fn write_packaging(dir: &Path, marker: &str) {
    std::fs::create_dir_all(dir.join("debian")).unwrap();
    std::fs::write(dir.join("debian/control"), CONTROL).unwrap();
    std::fs::write(dir.join("debian/marker"), format!("{marker}\n")).unwrap();
    std::fs::write(dir.join("marker"), "the packaging repository's own tree").unwrap();
}

/// Writes packaging into a repository and commits it.
fn commit_packaging(dir: &Path, marker: &str) {
    write_packaging(dir, marker);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", marker]);
}

/// The marker a packaging overlay left in a resolved tree.
fn packaging_marker(tree: &Path) -> String {
    std::fs::read_to_string(tree.join("debian/marker"))
        .unwrap()
        .trim()
        .to_string()
}

/// `component` with its packaging overlaid from a git repository.
fn overlaid_from_git(mut component: Component, repository: &Path) -> Component {
    component.packaging = Some(Source {
        git: Some(repository.to_string_lossy().into_owned()),
        ..Source::default()
    });
    component
}

/// `component` with its packaging overlaid from a tree on disk.
fn overlaid_from_path(mut component: Component, path: &Path) -> Component {
    component.packaging = Some(Source {
        path: Some(path.to_path_buf()),
        ..Source::default()
    });
    component
}

/// `component`'s packaging overlay, taken from `subdir` within its source.
fn overlay_subdir(mut component: Component, subdir: &str) -> Component {
    component
        .packaging
        .as_mut()
        .expect("the component declares a packaging overlay")
        .subdir = Some(PathBuf::from(subdir));
    component
}

#[test]
fn a_branch_ref_advances_to_the_upstream_tip_on_re_resolve() {
    if !git_available() {
        return;
    }
    let root = scratch("branch-advance");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");
    git(&upstream, &["branch", "trackme"]);
    git(&upstream, &["checkout", "-q", "trackme"]);

    let resolver = resolver_in(&root, &root);
    let comp = component("pkg", &upstream, Some("trackme"));

    let tree = resolver.resolve(&comp).expect("first resolve").tree;
    assert_eq!(resolved_marker(&tree), "v1");

    // Advance the tracked branch upstream and re-resolve: the checkout must move
    // to the new tip, not stay pinned at the clone-time commit.
    commit_marker(&upstream, "v2");
    let tree = resolver.resolve(&comp).expect("second resolve").tree;
    assert_eq!(
        resolved_marker(&tree),
        "v2",
        "a branch ref must advance to the fetched upstream tip"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unset_ref_tracks_the_default_branch_on_re_resolve() {
    if !git_available() {
        return;
    }
    let root = scratch("default-advance");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");

    let resolver = resolver_in(&root, &root);
    let comp = component("pkg", &upstream, None);

    let tree = resolver.resolve(&comp).expect("first resolve").tree;
    assert_eq!(resolved_marker(&tree), "v1");

    commit_marker(&upstream, "v2");
    let tree = resolver.resolve(&comp).expect("second resolve").tree;
    assert_eq!(
        resolved_marker(&tree),
        "v2",
        "an unset ref must track the remote's default branch"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_pinned_commit_stays_put_when_upstream_advances() {
    if !git_available() {
        return;
    }
    let root = scratch("pinned-commit");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");
    let pinned = head(&upstream);

    let resolver = resolver_in(&root, &root);
    let comp = component("pkg", &upstream, Some(&pinned));

    let tree = resolver.resolve(&comp).expect("first resolve").tree;
    assert_eq!(resolved_marker(&tree), "v1");

    // Upstream moves on, but a commit pin is immutable: the re-resolve must
    // still produce the pinned tree.
    commit_marker(&upstream, "v2");
    let tree = resolver.resolve(&comp).expect("second resolve").tree;
    assert_eq!(
        resolved_marker(&tree),
        "v1",
        "a pinned commit must not follow upstream"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn resolve_reports_the_checked_out_commit_for_provenance() {
    if !git_available() {
        return;
    }
    let root = scratch("resolve-commit");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");
    let pinned = head(&upstream);

    let resolver = resolver_in(&root, &root);
    let comp = component("pkg", &upstream, Some(&pinned));

    // The resolved source is a single pinned git input naming the exact HEAD
    // the tree was checked out at, which the provenance manifest records and
    // the version stamp abbreviates.
    let resolved = resolver.resolve(&comp).expect("resolve");
    assert_eq!(
        resolved.source,
        Fingerprint::of(SourceInput::git(SourceRole::Source, &pinned)),
    );
    assert!(resolved.source.is_pinned());
    assert_eq!(resolved.source.git_commit(), Some(pinned.as_str()));
    assert_eq!(resolved.source.short(), &pinned[..7]);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_tree_holding_an_lfs_pointer_does_not_resolve() {
    if !git_available() {
        return;
    }
    let root = scratch("lfs-pointer");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");

    // A pointer as a checkout without LFS support leaves it. The repository
    // declares no LFS filter and there is no LFS server behind it, so the
    // content is unobtainable — which is exactly the shape of the failure this
    // guards: whatever the cause, a stub must never reach a build.
    std::fs::create_dir_all(upstream.join("res")).unwrap();
    std::fs::write(
        upstream.join("res/cities.bitcode-v0-6"),
        "version https://git-lfs.github.com/spec/v1\n\
         oid sha256:ae15dde8fe7213dd8f3cd2ca2fd4e226d8342bd06a6501613ecf111280fd4f7b\n\
         size 9402799\n",
    )
    .unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "add pointer"]);

    let resolver = resolver_in(&root, &root);
    let comp = component("pkg", &upstream, None);

    // Fails whether or not git-lfs is installed: without it the tool is
    // reported missing, with it the pull cannot produce the content and the
    // pointer survives the re-check.
    let err = resolver
        .resolve(&comp)
        .expect_err("a tree holding an LFS pointer must not resolve")
        .to_string();
    assert!(
        err.contains("cities.bitcode-v0-6"),
        "the failure must name the offending file, got: {err}"
    );
    assert!(
        err.contains("LFS") || err.contains("lfs"),
        "the failure must attribute the cause to Git LFS, got: {err}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_untracked_pointer_left_by_a_prior_build_does_not_fail_the_component() {
    if !git_available() {
        return;
    }
    let root = scratch("lfs-untracked");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");

    let resolver = resolver_in(&root, &root);
    let comp = component("pkg", &upstream, None);
    let tree = resolver.resolve(&comp).expect("first resolve").tree;

    // What a build leaves in the tree: vendored crates, which `git checkout
    // --force` does not remove. One of them opening with a pointer signature is
    // plausible — a crate vendored from a repository that itself uses LFS — and
    // it must not fail the component, because the component's own repository
    // has never heard of that file and could never pull it. The failure would
    // be permanent and would name a path with no connection to the component.
    let vendored = tree.join("vendor/some-crate/assets/mesh.bin");
    std::fs::create_dir_all(vendored.parent().unwrap()).unwrap();
    std::fs::write(
        &vendored,
        "version https://git-lfs.github.com/spec/v1\n\
         oid sha256:ae15dde8fe7213dd8f3cd2ca2fd4e226d8342bd06a6501613ecf111280fd4f7b\n\
         size 9402799\n",
    )
    .unwrap();

    resolver
        .resolve(&comp)
        .expect("a prior build's vendored files are not the component's assets");
    assert!(vendored.is_file(), "the resolve must leave the tree alone");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_path_source_is_copied_rather_than_built_where_it_lies() {
    let root = scratch("path-copy");
    let upstream = root.join("working-tree");
    write_tree(&upstream, "v1");

    let resolver = resolver_in(&root, &root);
    let comp = path_component("pkg", &upstream);
    let tree = resolver.resolve(&comp).expect("resolve").tree;

    // The resolved tree is under the work directory, not the tree the recipe
    // named — which is what keeps the vendor pass, which binds it read-write and
    // runs upstream's `debian/rules clean` in it, out of someone's working
    // directory.
    assert_eq!(tree, root.join("sources/pkg"));
    assert_eq!(resolved_marker(&tree), "v1");
    assert!(tree.join("debian/control").is_file());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_path_source_resolves_relative_to_the_recipe_directory() {
    // A recipe kept beside the trees it builds names them relatively, so the
    // pair moves together and does not depend on where src2deb was invoked.
    let root = scratch("path-relative");
    let recipe_dir = root.join("recipes/local");
    std::fs::create_dir_all(&recipe_dir).unwrap();
    write_tree(&root.join("trees/pkg"), "relative");

    let resolver = resolver_in(&root, &recipe_dir);
    let comp = path_component("pkg", Path::new("../../trees/pkg"));
    let tree = resolver.resolve(&comp).expect("resolve").tree;
    assert_eq!(resolved_marker(&tree), "relative");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_path_source_is_recorded_as_an_unpinned_input() {
    let root = scratch("path-unpinned");
    let upstream = root.join("working-tree");
    write_tree(&upstream, "v1");

    let resolver = resolver_in(&root, &root);
    let resolved = resolver
        .resolve(&path_component("pkg", &upstream))
        .expect("resolve");

    // Nothing about a path says what the tree held, so the record must not read
    // as a reproducible build: the manifest carries `pinned = false`, the
    // version stamp carries a marker rather than a revision, and
    // `--skip-published` will not skip it.
    assert!(!resolved.source.is_pinned());
    assert_eq!(resolved.source.short(), "local");
    assert_eq!(resolved.source.git_commit(), None);
    // The path itself is canonical, so the manifest names one path however the
    // recipe reached it.
    assert_eq!(
        resolved.source,
        Fingerprint::of(SourceInput::path(
            SourceRole::Source,
            upstream.canonicalize().unwrap().to_string_lossy()
        )),
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_re_resolved_path_source_carries_nothing_over_from_the_run_before() {
    // The guarantee `git checkout --force` gives a git source: a file deleted
    // upstream really disappears, and a prior run's `vendor.tar` is not handed
    // to the next one as though upstream had shipped it.
    let root = scratch("path-fresh");
    let upstream = root.join("working-tree");
    write_tree(&upstream, "v1");
    std::fs::write(upstream.join("goes-away"), "temporary").unwrap();

    let resolver = resolver_in(&root, &root);
    let comp = path_component("pkg", &upstream);
    let tree = resolver.resolve(&comp).expect("first resolve").tree;
    assert!(tree.join("goes-away").is_file());
    std::fs::write(tree.join("vendor.tar"), "what pass 1 leaves").unwrap();

    std::fs::remove_file(upstream.join("goes-away")).unwrap();
    std::fs::write(upstream.join("marker"), "v2").unwrap();
    let tree = resolver.resolve(&comp).expect("second resolve").tree;

    assert_eq!(resolved_marker(&tree), "v2");
    assert!(!tree.join("goes-away").exists(), "a deleted file survived");
    assert!(!tree.join("vendor.tar").exists(), "prior output survived");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_path_source_copy_keeps_symlinks_and_executable_bits() {
    use std::os::unix::fs::PermissionsExt;

    let root = scratch("path-modes");
    let upstream = root.join("working-tree");
    write_tree(&upstream, "v1");
    // `debian/rules` must stay executable, and a link within the tree must stay
    // a link rather than becoming a second copy of its target.
    let rules = upstream.join("debian/rules");
    std::fs::write(&rules, "#!/usr/bin/make -f\n").unwrap();
    std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("marker", upstream.join("marker-link")).unwrap();

    let resolver = resolver_in(&root, &root);
    let tree = resolver
        .resolve(&path_component("pkg", &upstream))
        .expect("resolve")
        .tree;

    let mode = std::fs::metadata(tree.join("debian/rules"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755, "debian/rules lost its executable bit");
    let link = tree.join("marker-link");
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "a symlink was copied as a file",
    );
    assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("marker"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_path_source_without_a_debian_tree_does_not_resolve() {
    let root = scratch("path-no-control");
    let upstream = root.join("working-tree");
    std::fs::create_dir_all(&upstream).unwrap();
    std::fs::write(upstream.join("marker"), "v1").unwrap();

    let resolver = resolver_in(&root, &root);
    let err = resolver
        .resolve(&path_component("pkg", &upstream))
        .expect_err("a tree with no debian/control is not a component")
        .to_string();
    assert!(err.contains("debian/control"), "{err}");
    // Refused before anything was copied, so a misdirected path costs nothing.
    assert!(
        !root.join("sources/pkg").exists(),
        "the tree was copied anyway"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_path_source_that_is_not_there_does_not_resolve() {
    let root = scratch("path-missing");
    let resolver = resolver_in(&root, &root);
    let err = resolver
        .resolve(&path_component("pkg", &root.join("nowhere")))
        .expect_err("a path that does not exist is not a source")
        .to_string();
    assert!(err.contains("source.path"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_path_source_holding_the_work_directory_does_not_resolve() {
    // The copy's destination lies inside the tree it would copy, so the copy
    // would walk into its own output. Refused rather than run.
    let root = scratch("path-overlap");
    write_tree(&root, "v1");

    let resolver = resolver_in(&root, &root);
    let err = resolver
        .resolve(&path_component("pkg", &root))
        .expect_err("a source containing its own copy must be refused")
        .to_string();
    assert!(err.contains("copied into itself"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_path_source_holding_an_lfs_pointer_does_not_resolve() {
    if !git_available() {
        return;
    }
    let root = scratch("path-lfs");
    let upstream = root.join("working-tree");
    init_repo(&upstream);
    write_tree(&upstream, "v1");
    std::fs::create_dir_all(upstream.join("res")).unwrap();
    std::fs::write(
        upstream.join("res/cities.bitcode-v0-6"),
        "version https://git-lfs.github.com/spec/v1\n\
         oid sha256:ae15dde8fe7213dd8f3cd2ca2fd4e226d8342bd06a6501613ecf111280fd4f7b\n\
         size 9402799\n",
    )
    .unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "add pointer"]);

    let resolver = resolver_in(&root, &root);
    let err = resolver
        .resolve(&path_component("pkg", &upstream))
        .expect_err("a tree holding an LFS pointer must not resolve")
        .to_string();
    assert!(err.contains("cities.bitcode-v0-6"), "{err}");
    // The tree belongs to whoever pointed src2deb at it, so the remedy is named
    // rather than performed.
    assert!(err.contains("git lfs pull"), "{err}");
    assert!(
        err.contains(&upstream.canonicalize().unwrap().display().to_string()),
        "the failure must name the tree to run it in: {err}",
    );
    // ...and nothing was written into that tree, nor copied out of it.
    assert!(!upstream.join(".gitattributes").exists());
    assert!(!root.join("sources/pkg").exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_plain_directory_inside_a_repository_is_not_scanned_for_pointers() {
    if !git_available() {
        return;
    }
    // A work directory is often itself inside a checkout. The scan must be
    // bounded to the tree the recipe named, not widened to the repository that
    // happens to enclose it, or an unrelated file would fail the component.
    let root = scratch("path-lfs-bounded");
    init_repo(&root);
    std::fs::write(
        root.join("elsewhere.bin"),
        "version https://git-lfs.github.com/spec/v1\n\
         oid sha256:ae15dde8fe7213dd8f3cd2ca2fd4e226d8342bd06a6501613ecf111280fd4f7b\n\
         size 9402799\n",
    )
    .unwrap();
    let upstream = root.join("working-tree");
    write_tree(&upstream, "v1");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "everything"]);

    let resolver = resolver_in(&root, &root);
    resolver
        .resolve(&path_component("pkg", &upstream))
        .expect("a pointer outside the component's tree is not the component's");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_patch_series_is_applied_over_a_path_source() {
    let root = scratch("patch-path");
    let upstream = root.join("working-tree");
    write_tree(&upstream, "v1");
    std::fs::write(upstream.join(PATCHABLE), BEFORE).unwrap();
    write_patch(&root, "one.patch", &rewrite_patch("after one"));
    write_patch(&root, "two.patch", ADDS_A_FILE);

    let resolver = resolver_in(&root, &root);
    let comp = patched(
        path_component("pkg", &upstream),
        &["one.patch", "two.patch"],
    );
    let resolved = resolver.resolve(&comp).expect("resolve");

    assert_eq!(patched_content(&resolved.tree), "after one\n");
    assert_eq!(
        std::fs::read_to_string(resolved.tree.join("added.txt")).unwrap(),
        "added by a patch\n",
    );
    // Applied to src2deb's copy, never to the tree the recipe named.
    assert_eq!(
        std::fs::read_to_string(upstream.join(PATCHABLE)).unwrap(),
        BEFORE,
    );
    assert!(!upstream.join("added.txt").exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_patch_series_is_a_pinned_input_of_its_own() {
    let root = scratch("patch-fingerprint");
    let upstream = root.join("working-tree");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");
    std::fs::write(upstream.join(PATCHABLE), BEFORE).unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "patchable"]);
    if !git_available() {
        return;
    }
    write_patch(&root, "one.patch", &rewrite_patch("after one"));

    let resolver = resolver_in(&root, &root);
    let plain = component("pkg", &upstream, None);
    let with_patch = patched(plain.clone(), &["one.patch"]);

    let bare = resolver.resolve(&plain).expect("unpatched resolve").source;
    let series = resolver
        .resolve(&with_patch)
        .expect("patched resolve")
        .source;

    // The commit is unchanged, so a fingerprint over the commit alone could not
    // tell the two builds apart. The series is a second input, and a pinned one:
    // its digest names exactly the patches that were applied.
    assert_eq!(bare.len(), 1);
    assert_eq!(series.len(), 2);
    assert_eq!(series.git_commit(), bare.git_commit());
    assert_ne!(bare, series);
    assert!(series.is_pinned());
    assert_eq!(series.inputs()[1].kind(), src2deb::SourceKind::Patches);
    // The version carries both, so a patched package is distinguishable from an
    // unpatched one built from the same revision on the same day.
    assert_eq!(
        series.short(),
        format!("{}.{}", bare.short(), &series.inputs()[1].value()[..7]),
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn editing_or_reordering_a_patch_changes_what_the_component_is_built_from() {
    let root = scratch("patch-changes");
    let upstream = root.join("working-tree");
    write_tree(&upstream, "v1");
    std::fs::write(upstream.join(PATCHABLE), BEFORE).unwrap();
    write_patch(&root, "one.patch", &rewrite_patch("after one"));
    write_patch(&root, "two.patch", ADDS_A_FILE);

    let resolver = resolver_in(&root, &root);
    let base = path_component("pkg", &upstream);
    let one = resolver
        .resolve(&patched(base.clone(), &["one.patch"]))
        .expect("resolve")
        .source;

    // Adding a patch to the series changes it...
    let both = resolver
        .resolve(&patched(base.clone(), &["one.patch", "two.patch"]))
        .expect("resolve")
        .source;
    assert_ne!(one, both);
    // ...and so does reordering the same patches, since a series is applied in
    // the order it is declared and two orders need not produce the same tree.
    let reversed = resolver
        .resolve(&patched(base.clone(), &["two.patch", "one.patch"]))
        .expect("resolve")
        .source;
    assert_ne!(both, reversed);

    // Editing a patch in place changes it too, which is what makes
    // `--skip-published` rebuild after a fix is amended.
    write_patch(&root, "one.patch", &rewrite_patch("after one, amended"));
    let amended = resolver
        .resolve(&patched(base.clone(), &["one.patch"]))
        .expect("resolve")
        .source;
    assert_ne!(one, amended);

    // Renaming a patch does not: the same patches in the same order produce the
    // same tree, so the component was built from the same thing.
    write_patch(&root, "renamed.patch", &rewrite_patch("after one, amended"));
    let renamed = resolver
        .resolve(&patched(base, &["renamed.patch"]))
        .expect("resolve")
        .source;
    assert_eq!(amended, renamed);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_patch_that_does_not_apply_fails_the_component() {
    let root = scratch("patch-conflict");
    let upstream = root.join("working-tree");
    write_tree(&upstream, "v1");
    std::fs::write(upstream.join(PATCHABLE), "something else entirely\n").unwrap();
    write_patch(&root, "one.patch", &rewrite_patch("after one"));

    let resolver = resolver_in(&root, &root);
    let err = resolver
        .resolve(&patched(path_component("pkg", &upstream), &["one.patch"]))
        .expect_err("a patch that does not apply must fail the component")
        .to_string();
    // The failure names the patch, not just the file it could not change.
    assert!(err.contains("one.patch"), "{err}");
    assert!(err.contains("does not apply"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_patch_that_is_not_there_fails_the_component() {
    let root = scratch("patch-missing");
    let upstream = root.join("working-tree");
    write_tree(&upstream, "v1");

    let resolver = resolver_in(&root, &root);
    let err = resolver
        .resolve(&patched(
            path_component("pkg", &upstream),
            &["nowhere.patch"],
        ))
        .expect_err("a patch file that does not exist must fail the component")
        .to_string();
    assert!(err.contains("nowhere.patch"), "{err}");
    assert!(err.contains("cannot be read"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_patched_git_checkout_re_resolves_across_runs() {
    if !git_available() {
        return;
    }
    // The case a persistent checkout makes awkward: `git checkout --force`
    // restores every tracked file the series modified, but leaves the files it
    // created — so a second run would find them already there and refuse to
    // create them again.
    let root = scratch("patch-rerun");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");
    std::fs::write(upstream.join(PATCHABLE), BEFORE).unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "patchable"]);
    write_patch(&root, "one.patch", &rewrite_patch("after one"));
    write_patch(&root, "two.patch", ADDS_A_FILE);

    let resolver = resolver_in(&root, &root);
    let comp = patched(
        component("pkg", &upstream, None),
        &["one.patch", "two.patch"],
    );

    let first = resolver.resolve(&comp).expect("first resolve");
    // What the vendor pass leaves in the tree, which the discard must not touch.
    std::fs::write(first.tree.join("vendor.tar"), "left by pass 1").unwrap();

    for run in 2..=3 {
        let again = resolver
            .resolve(&comp)
            .unwrap_or_else(|err| panic!("resolve {run} must succeed: {err}"));
        assert_eq!(patched_content(&again.tree), "after one\n");
        assert!(again.tree.join("added.txt").is_file());
        assert_eq!(again.source, first.source);
        assert!(
            again.tree.join("vendor.tar").is_file(),
            "the discard must leave the vendor pass's output alone",
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dropping_a_patch_drops_what_it_added() {
    if !git_available() {
        return;
    }
    // A file a series created is untracked, so nothing in a plain re-checkout
    // removes it — and the patch that named it is gone from the recipe, so this
    // run's series cannot name it either. Left there, it would be built into the
    // package: a change to the recipe the build silently does not follow.
    let root = scratch("patch-dropped");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");
    write_patch(&root, "adds.patch", ADDS_A_FILE);

    let resolver = resolver_in(&root, &root);
    let comp = component("pkg", &upstream, None);
    let tree = resolver
        .resolve(&patched(comp.clone(), &["adds.patch"]))
        .expect("resolve with the patch")
        .tree;
    assert!(tree.join("added.txt").is_file());
    // What the vendor pass leaves, which the cleanup must not reach.
    std::fs::write(tree.join("vendor.tar"), "left by pass 1").unwrap();

    let resolved = resolver
        .resolve(&comp)
        .expect("resolve with the patch dropped");
    assert!(
        !resolved.tree.join("added.txt").exists(),
        "a dropped patch left its file behind",
    );
    assert!(resolved.tree.join("vendor.tar").is_file());
    // The fingerprint is back to the bare revision, matching the tree.
    assert_eq!(resolved.source.len(), 1);

    // ...and the component is still buildable after that, rather than the
    // cleanup having left state that trips the next run.
    let again = resolver
        .resolve(&patched(comp, &["adds.patch"]))
        .expect("resolve with the patch restored");
    assert!(again.tree.join("added.txt").is_file());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_packaging_overlay_supplies_the_debian_tree_a_source_has_none_of() {
    if !git_available() {
        return;
    }
    // The case the feature exists for: upstream ships no packaging, and
    // someone else's repository does.
    let root = scratch("overlay-supplies");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_bare_marker(&upstream, "v1");
    let packaging = root.join("packaging-repo");
    init_repo(&packaging);
    commit_packaging(&packaging, "from packaging");

    let resolver = resolver_in(&root, &root);
    let comp = overlaid_from_git(component("pkg", &upstream, None), &packaging);
    let resolved = resolver.resolve(&comp).expect("resolve");

    // The tree is the component's own source, with the overlay's packaging in
    // it. The overlay's source is kept apart, since both are named for the
    // component and only one of them is what gets built.
    assert_eq!(resolved.tree, root.join("sources/pkg"));
    assert_eq!(resolved_marker(&resolved.tree), "v1");
    assert!(resolved.tree.join("debian/control").is_file());
    assert_eq!(packaging_marker(&resolved.tree), "from packaging");
    assert!(root.join("packaging/pkg/debian").is_dir());

    // Both revisions are recorded, and each says which part it played — the
    // only thing that tells two git inputs apart. `git_commit` reads the
    // component's own source, which is what packaging asking for the revision
    // it was built from means.
    assert_eq!(resolved.source.len(), 2);
    assert!(resolved.source.is_pinned());
    assert_eq!(resolved.source.git_commit(), Some(head(&upstream).as_str()));
    assert_eq!(resolved.source.inputs()[0].role(), SourceRole::Source);
    assert_eq!(resolved.source.inputs()[1].role(), SourceRole::Packaging);
    assert_eq!(resolved.source.inputs()[1].value(), head(&packaging));
    // The version carries both, so a package is legible back to the packaging
    // it was built with as well as the source.
    assert_eq!(
        resolved.source.short(),
        format!("{}.{}", &head(&upstream)[..7], &head(&packaging)[..7]),
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_packaging_overlay_contributes_nothing_outside_debian() {
    if !git_available() {
        return;
    }
    // A distribution's packaging repository ordinarily carries a copy of the
    // upstream tree beside its `debian/`, and that copy is not the source this
    // component is being built from — it is usually an older release of it.
    // Taking it would replace the component's source with upstream's last
    // packaged version, silently.
    let root = scratch("overlay-bounded");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_bare_marker(&upstream, "the source being built");
    let packaging = root.join("packaging-repo");
    init_repo(&packaging);
    commit_packaging(&packaging, "from packaging");

    let resolver = resolver_in(&root, &root);
    let resolved = resolver
        .resolve(&overlaid_from_git(
            component("pkg", &upstream, None),
            &packaging,
        ))
        .expect("resolve");

    assert_eq!(
        resolved_marker(&resolved.tree),
        "the source being built",
        "the overlay replaced a file outside debian/",
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_packaging_overlay_replaces_the_debian_tree_a_source_ships() {
    if !git_available() {
        return;
    }
    // Union with the source's own packaging would leave whatever the abandoned
    // one shipped beside the declared one — an install file naming a path the
    // new packaging never builds, a `patches/series` applied by a build that
    // was never asked to.
    let root = scratch("overlay-replaces");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");
    std::fs::write(upstream.join("debian/stray"), "the source's own\n").unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "stray"]);
    let packaging = root.join("packaging-repo");
    init_repo(&packaging);
    commit_packaging(&packaging, "from packaging");

    let resolver = resolver_in(&root, &root);
    let resolved = resolver
        .resolve(&overlaid_from_git(
            component("pkg", &upstream, None),
            &packaging,
        ))
        .expect("resolve");

    assert_eq!(packaging_marker(&resolved.tree), "from packaging");
    assert!(
        !resolved.tree.join("debian/stray").exists(),
        "the source's own packaging survived alongside the overlay",
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dropping_a_packaging_overlay_restores_the_sources_own_packaging() {
    if !git_available() {
        return;
    }
    // A recipe stops declaring an overlay when upstream starts shipping
    // packaging of its own. The overlay's files are untracked in the checkout,
    // so nothing in a plain re-checkout removes them, and the run would keep
    // building the packaging the recipe no longer names.
    let root = scratch("overlay-dropped");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");
    std::fs::write(upstream.join("debian/stray"), "the source's own\n").unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "stray"]);
    let packaging = root.join("packaging-repo");
    init_repo(&packaging);
    commit_packaging(&packaging, "from packaging");

    let resolver = resolver_in(&root, &root);
    let plain = component("pkg", &upstream, None);
    let overlaid = overlaid_from_git(plain.clone(), &packaging);

    let tree = resolver.resolve(&overlaid).expect("resolve overlaid").tree;
    assert_eq!(packaging_marker(&tree), "from packaging");
    // What the vendor pass leaves in the tree, which the cleanup must not touch.
    std::fs::write(tree.join("vendor.tar"), "left by pass 1").unwrap();

    let resolved = resolver.resolve(&plain).expect("resolve with it dropped");
    assert_eq!(
        std::fs::read_to_string(resolved.tree.join("debian/stray")).unwrap(),
        "the source's own\n",
        "the source's own packaging did not come back",
    );
    assert!(
        !resolved.tree.join("debian/marker").exists(),
        "a dropped overlay left its packaging behind",
    );
    assert!(resolved.tree.join("vendor.tar").is_file());
    assert_eq!(resolved.source.len(), 1);

    // ...and declaring it again still works, rather than the cleanup having
    // left state that trips the next run.
    let again = resolver.resolve(&overlaid).expect("resolve overlaid again");
    assert_eq!(packaging_marker(&again.tree), "from packaging");
    assert!(!again.tree.join("debian/stray").exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_overlaid_checkout_re_resolves_unchanged_across_runs() {
    if !git_available() {
        return;
    }
    let root = scratch("overlay-rerun");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_bare_marker(&upstream, "v1");
    let packaging = root.join("packaging-repo");
    init_repo(&packaging);
    commit_packaging(&packaging, "from packaging");

    let resolver = resolver_in(&root, &root);
    let comp = overlaid_from_git(component("pkg", &upstream, None), &packaging);
    let first = resolver.resolve(&comp).expect("first resolve");
    std::fs::write(first.tree.join("vendor.tar"), "left by pass 1").unwrap();

    for run in 2..=3 {
        let again = resolver
            .resolve(&comp)
            .unwrap_or_else(|err| panic!("resolve {run} must succeed: {err}"));
        assert_eq!(packaging_marker(&again.tree), "from packaging");
        assert_eq!(again.source, first.source);
        assert!(
            again.tree.join("vendor.tar").is_file(),
            "the discard must leave the vendor pass's output alone",
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn moving_the_packaging_repository_changes_what_the_component_is_built_from() {
    if !git_available() {
        return;
    }
    // The overlay is a build input like any other, so a packaging change has to
    // reach the version stamp and `--skip-published` even though the component's
    // own source has not moved.
    let root = scratch("overlay-moves");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_bare_marker(&upstream, "v1");
    let packaging = root.join("packaging-repo");
    init_repo(&packaging);
    commit_packaging(&packaging, "first");

    let resolver = resolver_in(&root, &root);
    let comp = overlaid_from_git(component("pkg", &upstream, None), &packaging);
    let before = resolver.resolve(&comp).expect("resolve").source;

    commit_packaging(&packaging, "second");
    let after = resolver.resolve(&comp).expect("re-resolve").source;

    assert_ne!(before, after);
    assert_eq!(after.git_commit(), before.git_commit());
    assert_eq!(after.inputs()[1].value(), head(&packaging));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_packaging_overlay_from_a_tree_on_disk_is_read_where_it_lies() {
    let root = scratch("overlay-path");
    let upstream = root.join("working-tree");
    write_bare_tree(&upstream, "v1");
    let packaging = root.join("packaging");
    write_packaging(&packaging, "from a tree on disk");

    let resolver = resolver_in(&root, &root);
    let resolved = resolver
        .resolve(&overlaid_from_path(
            path_component("pkg", &upstream),
            &packaging,
        ))
        .expect("resolve");

    assert_eq!(packaging_marker(&resolved.tree), "from a tree on disk");
    // Nothing writes to a packaging source, so a path one is read where it is
    // rather than copied under the work directory as a source is.
    assert!(
        !root.join("packaging/pkg").exists(),
        "a path packaging source was copied for no reason",
    );

    // Two unpinned inputs: a path says where a tree was read from and nothing
    // about what it held, whichever of a component's trees it names. The role
    // is what tells them apart, since the version marks both the same way.
    assert_eq!(resolved.source.len(), 2);
    assert!(!resolved.source.is_pinned());
    assert_eq!(resolved.source.short(), "local.local");
    assert_eq!(resolved.source.inputs()[1].role(), SourceRole::Packaging);
    assert_eq!(
        resolved.source.inputs()[1].value(),
        packaging.canonicalize().unwrap().to_string_lossy(),
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_packaging_subdir_names_the_directory_holding_debian() {
    let root = scratch("overlay-subdir");
    let upstream = root.join("working-tree");
    write_bare_tree(&upstream, "v1");
    let packaging = root.join("packaging");
    write_packaging(&packaging.join("pkg"), "from a subdirectory");

    let resolver = resolver_in(&root, &root);
    let comp = overlay_subdir(
        overlaid_from_path(path_component("pkg", &upstream), &packaging),
        "pkg",
    );
    let resolved = resolver.resolve(&comp).expect("resolve");
    assert_eq!(packaging_marker(&resolved.tree), "from a subdirectory");

    // The record names the tree the recipe declared, as a path source does; the
    // recipe stays the authority for which part of it was taken.
    assert_eq!(
        resolved.source.inputs()[1].value(),
        packaging.canonicalize().unwrap().to_string_lossy(),
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_packaging_subdir_that_is_not_there_names_the_setting_that_declared_it() {
    let root = scratch("overlay-subdir-missing");
    let upstream = root.join("working-tree");
    write_bare_tree(&upstream, "v1");
    let packaging = root.join("packaging");
    write_packaging(&packaging, "unused");

    let resolver = resolver_in(&root, &root);
    let comp = overlay_subdir(
        overlaid_from_path(path_component("pkg", &upstream), &packaging),
        "nowhere",
    );
    let err = resolver
        .resolve(&comp)
        .expect_err("a subdir the source does not hold is not resolvable")
        .to_string();
    assert!(err.contains("packaging.subdir"), "{err}");
    assert!(err.contains("nowhere"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_packaging_source_carrying_no_debian_directory_does_not_resolve() {
    let root = scratch("overlay-no-debian");
    let upstream = root.join("working-tree");
    write_bare_tree(&upstream, "v1");
    let packaging = root.join("packaging");
    // The mistake the error has to name: pointed one level too deep, at the
    // `debian/` itself rather than the directory holding it.
    write_packaging(&packaging, "unused");

    let resolver = resolver_in(&root, &root);
    let err = resolver
        .resolve(&overlaid_from_path(
            path_component("pkg", &upstream),
            &packaging.join("debian"),
        ))
        .expect_err("a packaging source with no debian/ supplies no packaging")
        .to_string();
    assert!(err.contains("no debian directory"), "{err}");
    assert!(err.contains("packaging.subdir"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_component_may_not_take_its_packaging_from_its_own_source_tree() {
    if !git_available() {
        return;
    }
    // The destination is removed before the copy, so a packaging source that is
    // the component's own tree would be deleted and then read.
    let root = scratch("overlay-overlap");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");

    let resolver = resolver_in(&root, &root);
    let plain = component("pkg", &upstream, None);
    resolver
        .resolve(&plain)
        .expect("resolve to establish the tree");

    let err = resolver
        .resolve(&overlaid_from_path(plain, &root.join("sources/pkg")))
        .expect_err("a tree may not be overlaid onto itself")
        .to_string();
    assert!(err.contains("sit inside one another"), "{err}");
    // ...and the tree it would have removed is still there.
    assert!(root.join("sources/pkg/debian/control").is_file());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_patch_series_applies_over_the_packaging_an_overlay_supplied() {
    // The assembly order the guide states: source, then overlay, then patches.
    // A patch that could not reach the overlay's files would leave the one
    // input a recipe has for fixing packaging it does not control.
    let root = scratch("overlay-patched");
    let upstream = root.join("working-tree");
    write_bare_tree(&upstream, "v1");
    let packaging = root.join("packaging");
    write_packaging(&packaging, "from packaging");
    write_patch(
        &root,
        "fix.patch",
        "--- a/debian/marker\n+++ b/debian/marker\n@@ -1 +1 @@\n\
         -from packaging\n+patched after the overlay\n",
    );

    let resolver = resolver_in(&root, &root);
    let comp = patched(
        overlaid_from_path(path_component("pkg", &upstream), &packaging),
        &["fix.patch"],
    );
    let resolved = resolver.resolve(&comp).expect("resolve");

    assert_eq!(
        packaging_marker(&resolved.tree),
        "patched after the overlay"
    );
    // Applied to the assembled tree, never to the packaging the recipe named.
    assert_eq!(
        std::fs::read_to_string(packaging.join("debian/marker")).unwrap(),
        "from packaging\n",
    );
    // Three inputs, in assembly order.
    assert_eq!(resolved.source.len(), 3);
    assert_eq!(
        resolved.source.inputs()[2].kind(),
        src2deb::SourceKind::Patches
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_patch_applies_even_when_the_work_directory_sits_inside_a_repository() {
    if !git_available() {
        return;
    }
    // A work directory inside a checkout is an ordinary arrangement, and it used
    // to change what a patch did. `git apply` run from a subdirectory of a
    // repository prefixes every patch path with that subdirectory, and then
    // silently skips a git-format patch that creates a file, because the name in
    // its `diff --git` header does not carry the prefix it now expects — exiting
    // zero having written nothing. The package would then carry a version saying
    // it was patched over contents saying it was not.
    let root = scratch("patch-enclosed");
    init_repo(&root);
    let upstream = root.join("working-tree");
    write_tree(&upstream, "v1");
    std::fs::write(upstream.join(PATCHABLE), BEFORE).unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "enclosing"]);
    write_patch(&root, "adds.patch", ADDS_A_FILE);

    let resolver = resolver_in(&root, &root);
    let resolved = resolver
        .resolve(&patched(path_component("pkg", &upstream), &["adds.patch"]))
        .expect("resolve");

    assert_eq!(
        std::fs::read_to_string(resolved.tree.join("added.txt")).unwrap(),
        "added by a patch\n",
        "the enclosing repository must not change what a patch does",
    );

    let _ = std::fs::remove_dir_all(&root);
}
