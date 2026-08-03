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

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use src2deb::recipe::Component;
use src2deb::source::SourceResolver;
use src2deb::version::{BuildStamp, ChangelogHead, parse_changelog};
use src2deb::{Fingerprint, Source, SourceInput, SourceRole, VersionFrom};

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
        ..Component::default()
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
        ..Component::default()
    }
}

/// The stamp a resolve dates a synthesized changelog with.
///
/// Fixed rather than `now()`, so a test reading one back reads the same text
/// every run, and shared so a resolver can borrow it without every test
/// carrying one of its own.
fn stamp() -> &'static BuildStamp {
    static STAMP: OnceLock<BuildStamp> = OnceLock::new();
    // 2026-07-31, the date the version tests stamp with.
    STAMP.get_or_init(|| BuildStamp::at("deb13", 1_785_456_000))
}

/// A resolver whose relative paths are taken from `recipe_dir`, declaring no
/// recipe-level maintainer — so a component that needs one takes it from its
/// own `debian/control`.
fn resolver_in(root: &Path, recipe_dir: &Path) -> SourceResolver<'static> {
    SourceResolver::new(root, recipe_dir, None, stamp())
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
///
/// It declares a `Maintainer`, as Debian policy requires of any control file, so
/// a component declaring a version has an identity to sign a synthesized
/// changelog with without the recipe restating one.
const CONTROL: &str = "Source: pkg\nMaintainer: Control Owner <control@example.invalid>\n\
     \nPackage: pkg\nArchitecture: any\n";

/// The identity [`CONTROL`] declares.
const CONTROL_MAINTAINER: &str = "Control Owner <control@example.invalid>";

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

/// `component` with the upstream version its recipe states.
fn versioned(mut component: Component, version: &str) -> Component {
    component.version = Some(version.to_string());
    component
}

/// `component` with its upstream version derived from `git describe`.
fn described(mut component: Component) -> Component {
    component.version_from = Some(VersionFrom::GitDescribe);
    component
}

/// The head of a resolved tree's `debian/changelog`, which is what the version
/// stamp reads and so what a declared version has to produce.
fn changelog_head(tree: &Path) -> ChangelogHead {
    let text = std::fs::read_to_string(tree.join("debian/changelog"))
        .unwrap_or_else(|err| panic!("{}: {err}", tree.join("debian/changelog").display()));
    parse_changelog(&text).expect("the resolved tree holds a well-formed changelog")
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

    // The overlay is identified by what it held rather than by where it was
    // read from — a digest over the `debian/` tree it supplied — so the version
    // names the packaging rather than marking it `local`.
    assert_eq!(resolved.source.len(), 2);
    let overlay = &resolved.source.inputs()[1];
    assert_eq!(overlay.role(), SourceRole::Packaging);
    assert_eq!(overlay.kind(), src2deb::SourceKind::Tree);
    assert!(overlay.is_pinned());
    assert_eq!(overlay.value().len(), 64);
    assert_eq!(
        resolved.source.short(),
        format!("local.{}", &overlay.value()[..7])
    );
    // The component's own source is still a path, and one unpinned input is
    // enough to leave the whole build uncomparable.
    assert!(!resolved.source.is_pinned());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_path_packaging_overlay_is_identified_by_what_it_holds_not_where_it_is() {
    // The property that makes a directory beside the recipe as good an input as
    // a repository: editing it is a different build, moving it is not.
    let root = scratch("overlay-digest");
    let upstream = root.join("working-tree");
    write_bare_tree(&upstream, "v1");
    let packaging = root.join("packaging");
    write_packaging(&packaging, "first");

    let resolver = resolver_in(&root, &root);
    let comp = overlaid_from_path(path_component("pkg", &upstream), &packaging);
    let before = resolver.resolve(&comp).expect("resolve").source;

    // Editing the packaging is a different build...
    write_packaging(&packaging, "second");
    let edited = resolver.resolve(&comp).expect("re-resolve").source;
    assert_ne!(before, edited);

    // ...and so is changing what `debian/rules` may do, which is the one mode
    // bit the digest carries.
    let rules = packaging.join("debian/rules");
    std::fs::write(&rules, "#!/usr/bin/make -f\n").unwrap();
    let plain = resolver.resolve(&comp).expect("re-resolve").source;
    std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executable = resolver.resolve(&comp).expect("re-resolve").source;
    assert_ne!(plain, executable);

    // Moving the same packaging elsewhere is not: the digest is over what the
    // directory holds, so a recipe that relocates its packaging does not
    // republish every component that uses it.
    let moved = root.join("elsewhere/packaging");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::rename(&packaging, &moved).unwrap();
    let relocated = resolver
        .resolve(&overlaid_from_path(
            path_component("pkg", &upstream),
            &moved,
        ))
        .expect("resolve from the new location")
        .source;
    assert_eq!(relocated, executable);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_path_packaging_overlay_is_identified_by_debian_alone() {
    // Only `debian/` is copied out of an overlay, so only `debian/` may reach
    // its identity: a file beside it that never enters the build must not
    // provoke a rebuild of everything that uses the directory.
    let root = scratch("overlay-digest-bounded");
    let upstream = root.join("working-tree");
    write_bare_tree(&upstream, "v1");
    let packaging = root.join("packaging");
    write_packaging(&packaging, "the packaging");

    let resolver = resolver_in(&root, &root);
    let comp = overlaid_from_path(path_component("pkg", &upstream), &packaging);
    let before = resolver.resolve(&comp).expect("resolve").source;

    std::fs::write(packaging.join("README.md"), "notes for a human\n").unwrap();
    std::fs::create_dir_all(packaging.join("notes")).unwrap();
    let after = resolver.resolve(&comp).expect("re-resolve").source;
    assert_eq!(before, after);

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

    // The record names the `debian/` tree that was actually taken, so a subdir
    // is not something a reader of the manifest has to apply for themselves.
    let taken = resolver
        .resolve(&overlaid_from_path(
            path_component("pkg", &upstream),
            &packaging.join("pkg"),
        ))
        .expect("resolve the same packaging named directly")
        .source;
    assert_eq!(resolved.source.inputs()[1], taken.inputs()[1]);

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

#[test]
fn a_declared_version_gives_a_component_the_changelog_its_packaging_lacks() {
    if !git_available() {
        return;
    }
    // The case the feature exists for: packaging complete enough to build —
    // a control, a rules — with no release history behind it, so the version
    // stamp has nothing to extend.
    let root = scratch("declared-version");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");
    assert!(!upstream.join("debian/changelog").exists());

    let resolver = resolver_in(&root, &root);
    let resolved = resolver
        .resolve(&versioned(component("pkg", &upstream, None), "1.2.3"))
        .expect("resolve");

    // The tree now holds a changelog the stamping path can read, naming the
    // source package `debian/control` declares and the version the recipe did.
    let entry = changelog_head(&resolved.tree);
    assert_eq!(entry.source, "pkg");
    assert_eq!(entry.version, "1.2.3");
    // The identity comes from `debian/control`, which Debian policy makes
    // mandatory — so an ordinary recipe declares no identity at all.
    assert_eq!(entry.maintainer, CONTROL_MAINTAINER);
    // ...and the resolve reports the version, which is what a later run
    // compares against so that editing it rebuilds.
    assert_eq!(resolved.version.as_deref(), Some("1.2.3"));

    // It is not an input: the fingerprint still names the one tree the build
    // consumed, and the version stamp is unchanged by it.
    assert_eq!(resolved.source.len(), 1);
    assert_eq!(resolved.source.git_commit(), Some(head(&upstream).as_str()));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_declared_identity_outranks_the_one_control_declares() {
    if !git_available() {
        return;
    }
    // Three places an identity can come from, in the order they are consulted.
    let root = scratch("declared-maintainer");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");

    let stamp = BuildStamp::at("deb13", 1_785_456_000);
    let recipe_owner = "Recipe Owner <recipe@example.invalid>";
    let with_recipe_identity = SourceResolver::new(&root, &root, Some(recipe_owner), &stamp);

    let base = versioned(component("pkg", &upstream, None), "1.2.3");
    let resolved = with_recipe_identity.resolve(&base).expect("resolve");
    assert_eq!(changelog_head(&resolved.tree).maintainer, recipe_owner);

    let mut own = base.clone();
    own.maintainer = Some("Component Owner <component@example.invalid>".to_string());
    let resolved = with_recipe_identity.resolve(&own).expect("resolve");
    assert_eq!(
        changelog_head(&resolved.tree).maintainer,
        "Component Owner <component@example.invalid>",
    );

    // With neither declared, control answers — which is the ordinary case.
    let resolved = resolver_in(&root, &root).resolve(&base).expect("resolve");
    assert_eq!(
        changelog_head(&resolved.tree).maintainer,
        CONTROL_MAINTAINER
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_declared_version_with_no_identity_anywhere_does_not_resolve() {
    if !git_available() {
        return;
    }
    // src2deb never invents an identity. With nothing to reuse, the component
    // fails and names the setting that would fix it, rather than signing the
    // entry with something made up.
    let root = scratch("no-identity");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");
    std::fs::write(
        upstream.join("debian/control"),
        "Source: pkg\n\nPackage: pkg\nArchitecture: any\n",
    )
    .unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "drop the maintainer"]);

    let err = resolver_in(&root, &root)
        .resolve(&versioned(component("pkg", &upstream, None), "1.2.3"))
        .expect_err("an entry cannot be signed by nobody");
    let message = err.to_string();
    assert!(message.contains("no maintainer"), "{message}");
    assert!(message.contains("set maintainer"), "{message}");
    // Nothing was written on the way to failing.
    assert!(!root.join("sources/pkg/debian/changelog").exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_control_identity_that_could_not_sign_a_trailer_is_refused() {
    if !git_available() {
        return;
    }
    // The field is mandatory but not validated by anything upstream of here, and
    // an entry signed with a malformed one does not parse — from a file the
    // recipe never wrote, which is the hard failure to diagnose.
    let root = scratch("bad-control-identity");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");
    std::fs::write(
        upstream.join("debian/control"),
        "Source: pkg\nMaintainer: Someone\n\nPackage: pkg\nArchitecture: any\n",
    )
    .unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "malformed maintainer"]);

    let err = resolver_in(&root, &root)
        .resolve(&versioned(component("pkg", &upstream, None), "1.2.3"))
        .expect_err("a malformed identity cannot sign an entry");
    let message = err.to_string();
    assert!(
        message.contains("Maintainer field in debian/control"),
        "{message}"
    );
    assert!(message.contains("Name <email>"), "{message}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_declared_version_replaces_the_changelog_a_source_ships() {
    if !git_available() {
        return;
    }
    // The same rule a packaging overlay follows: one authority for the version,
    // with no per-entry precedence to reason about. A recipe that declares a
    // version for a component whose upstream changelog is frozen gets the
    // version it asked for.
    let root = scratch("version-replaces");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");
    std::fs::write(
        upstream.join("debian/changelog"),
        "pkg (0.1.0-1) trixie; urgency=low\n\n  * Frozen in 2019.\n\n \
         -- Old Owner <old@example.invalid>  Mon, 14 Jan 2019 09:00:00 +0000\n",
    )
    .unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "a stale changelog"]);

    let resolver = resolver_in(&root, &root);
    let base = component("pkg", &upstream, None);
    // Left alone, the component keeps upstream's changelog and its identity.
    let resolved = resolver.resolve(&base).expect("resolve");
    assert_eq!(changelog_head(&resolved.tree).version, "0.1.0-1");
    assert_eq!(resolved.version, None);

    let resolved = resolver
        .resolve(&versioned(base.clone(), "2.5.0"))
        .expect("resolve");
    assert_eq!(changelog_head(&resolved.tree).version, "2.5.0");
    // Replaced, not prepended: nothing of the old entry is left to contradict
    // the declared one.
    let text = std::fs::read_to_string(resolved.tree.join("debian/changelog")).unwrap();
    assert!(!text.contains("0.1.0-1"), "{text}");
    assert_eq!(text.matches("urgency=").count(), 1, "{text}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dropping_a_declared_version_restores_the_sources_own_changelog() {
    if !git_available() {
        return;
    }
    // A checkout persists between runs, and the file a declared version writes
    // into a tracked `debian/` is untracked — so without the assembly record,
    // a recipe that stops declaring a version would keep building the version
    // it used to declare.
    let root = scratch("version-dropped");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");

    let resolver = resolver_in(&root, &root);
    let base = component("pkg", &upstream, None);
    let resolved = resolver
        .resolve(&versioned(base.clone(), "1.2.3"))
        .expect("resolve");
    assert_eq!(changelog_head(&resolved.tree).version, "1.2.3");

    // The recipe stops declaring one. The source ships no changelog, so the
    // right answer is that there is none — which is what the version stamp
    // then reports, with the setting that would fix it.
    let resolved = resolver.resolve(&base).expect("resolve");
    assert_eq!(resolved.version, None);
    assert!(
        !resolved.tree.join("debian/changelog").exists(),
        "the dropped declaration left its changelog behind",
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_derived_version_comes_from_the_nearest_tag_and_the_distance_from_it() {
    if !git_available() {
        return;
    }
    let root = scratch("derived-version");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");
    git(&upstream, &["tag", "v1.2.3"]);

    let resolver = resolver_in(&root, &root);
    let comp = described(component("pkg", &upstream, None));
    let resolved = resolver.resolve(&comp).expect("resolve");
    // On the tag: the conventional leading `v` is not part of the version.
    assert_eq!(resolved.version.as_deref(), Some("1.2.3"));
    assert_eq!(changelog_head(&resolved.tree).version, "1.2.3");

    // Past it, the distance and the commit come with it — and no hyphen does,
    // so the Debian revision boundary stays where the stamp will put it.
    commit_bare_marker(&upstream, "v2");
    let resolved = resolver.resolve(&comp).expect("resolve");
    let version = resolved.version.expect("a derived version");
    assert!(version.starts_with("1.2.3.1.g"), "{version}");
    assert!(!version.contains('-'), "{version}");
    assert_eq!(changelog_head(&resolved.tree).version, version);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_source_with_no_tag_to_describe_does_not_resolve_a_derived_version() {
    if !git_available() {
        return;
    }
    // Falling back to an abbreviated commit would produce a version that does
    // not order against the one before it, which is worse than not building.
    let root = scratch("derived-untagged");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_marker(&upstream, "v1");

    let err = resolver_in(&root, &root)
        .resolve(&described(component("pkg", &upstream, None)))
        .expect_err("an untagged source has no version to derive");
    let message = err.to_string();
    assert!(message.contains("git describe"), "{message}");
    assert!(message.contains("`version`"), "{message}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_derived_version_never_reads_a_repository_enclosing_the_work_directory() {
    if !git_available() {
        return;
    }
    // A work directory inside a checkout is an ordinary arrangement, and git
    // searches upward for a repository. Without a ceiling, an untagged
    // component would be versioned from whatever tag the enclosing repository
    // happened to carry — a wrong answer arrived at silently, where no answer
    // is loud.
    let enclosing = scratch("derived-enclosed");
    init_repo(&enclosing);
    std::fs::write(enclosing.join("readme"), "the enclosing repository\n").unwrap();
    git(&enclosing, &["add", "-A"]);
    git(&enclosing, &["commit", "-q", "-m", "enclosing"]);
    git(&enclosing, &["tag", "v9.9.9"]);

    // The component's source is a plain directory: copied into the work
    // directory, it carries no repository of its own.
    let upstream = enclosing.join("upstream");
    write_tree(&upstream, "v1");

    let root = enclosing.join("work");
    std::fs::create_dir_all(&root).unwrap();
    let err = resolver_in(&root, &root)
        .resolve(&described(path_component("pkg", &upstream)))
        .expect_err("the enclosing repository's tag is not this component's");
    assert!(!err.to_string().contains("9.9.9"), "{err}");

    let _ = std::fs::remove_dir_all(&enclosing);
}

#[test]
fn a_declared_version_serves_a_component_packaged_from_a_second_repository() {
    if !git_available() {
        return;
    }
    // The shape E9 exists to unblock, end to end: upstream ships no `debian/`,
    // the packaging comes from elsewhere and carries no changelog, and the
    // recipe supplies the version neither of them has.
    let root = scratch("declared-version-overlaid");
    let upstream = root.join("upstream");
    init_repo(&upstream);
    commit_bare_marker(&upstream, "v1");
    let packaging = root.join("packaging-repo");
    init_repo(&packaging);
    commit_packaging(&packaging, "from packaging");

    let comp = versioned(
        overlaid_from_git(component("pkg", &upstream, None), &packaging),
        "1.0.0",
    );
    let resolved = resolver_in(&root, &root).resolve(&comp).expect("resolve");

    let entry = changelog_head(&resolved.tree);
    assert_eq!(entry.version, "1.0.0");
    assert_eq!(entry.source, "pkg");
    // The changelog is written last, so it is signed from the control the whole
    // assembly settled on — the overlay's, here.
    assert_eq!(entry.maintainer, CONTROL_MAINTAINER);
    // Both trees are still the only inputs recorded.
    assert_eq!(resolved.source.len(), 2);
    assert_eq!(resolved.version.as_deref(), Some("1.0.0"));

    let _ = std::fs::remove_dir_all(&root);
}

/// Whether `curl` can be launched at all; the archive tests no-op when it
/// cannot, as the git ones do without `git`.
fn curl_available() -> bool {
    Command::new("curl")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Writes an uncompressed ustar archive at `path` holding `entries`, each a
/// path within the archive and its contents; a path ending in `/` is a
/// directory.
///
/// Written here rather than shelled out to `tar`, so the tests need no archiver
/// on the host and the bytes they exercise are fixed rather than whatever the
/// local `tar` happens to emit.
fn write_tar(path: &Path, entries: &[(&str, &str)]) {
    write_tar_in_format(path, entries, true);
}

/// Writes the same archive as [`write_tar`] in the pre-POSIX **v7** format: no
/// magic, no version, and a plain `0` type flag for a directory rather than
/// ustar's `5`.
///
/// The v7 header is a prefix of the ustar one, so the difference is entirely in
/// the fields left NUL. That is what makes the format hard to detect and worth a
/// test of its own: roughly one in twelve Debian `.orig.tar.*` files is still
/// one of these.
fn write_tar_v7(path: &Path, entries: &[(&str, &str)]) {
    write_tar_in_format(path, entries, false);
}

/// The body of [`write_tar`] and [`write_tar_v7`]. `ustar` selects between the
/// POSIX header and the pre-POSIX one.
fn write_tar_in_format(path: &Path, entries: &[(&str, &str)], ustar: bool) {
    /// Copies `text` into `header` at `offset`, leaving the rest of the field
    /// as the NUL padding a tar field takes.
    fn put(header: &mut [u8; 512], offset: usize, text: &str) {
        header[offset..offset + text.len()].copy_from_slice(text.as_bytes());
    }

    let mut archive = Vec::new();
    for (name, contents) in entries {
        let directory = name.ends_with('/');
        let size = if directory { 0 } else { contents.len() };
        let mut header = [0u8; 512];
        put(&mut header, 0, name);
        put(
            &mut header,
            100,
            &format!("{:07o}", if directory { 0o755 } else { 0o644 }),
        );
        put(&mut header, 108, "0000000");
        put(&mut header, 116, "0000000");
        put(&mut header, 124, &format!("{size:011o}"));
        put(&mut header, 136, "00000000000");
        // The checksum field counts as spaces while the checksum is summed.
        header[148..156].fill(b' ');
        // v7 has no directory type flag: a directory is a zero-length entry
        // whose name ends in `/`, which is how the format said it at the time.
        header[156] = if directory && ustar { b'5' } else { b'0' };
        if ustar {
            put(&mut header, 257, "ustar\0");
            put(&mut header, 263, "00");
        }
        let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        put(&mut header, 148, &format!("{sum:06o}\0"));

        archive.extend_from_slice(&header);
        if !directory {
            archive.extend_from_slice(contents.as_bytes());
            archive.resize(archive.len().div_ceil(512) * 512, 0);
        }
    }
    // Two zero blocks end an archive.
    archive.resize(archive.len() + 1024, 0);
    std::fs::write(path, archive).unwrap();
}

/// The SHA-256 of a file, as a recipe author reads one off a release.
fn digest(path: &Path) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(std::fs::read(path).unwrap());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A release archive laid out the way one is: everything under a single
/// directory named for the release, and no `debian/` of its own.
fn write_release(path: &Path) {
    write_tar(
        path,
        &[
            ("pkg-1.2.3/", ""),
            ("pkg-1.2.3/marker", "from the archive\n"),
            ("pkg-1.2.3/src/", ""),
            ("pkg-1.2.3/src/main.c", "int main(void) { return 0; }\n"),
        ],
    );
}

/// A component built from the archive at `path`, pinned by `sha256`.
fn archive_component(name: &str, path: &Path, sha256: &str) -> Component {
    Component {
        name: name.to_string(),
        source: Source {
            tarball: Some(format!("file://{}", path.display())),
            sha256: Some(sha256.to_string()),
            ..Source::default()
        },
        ..Component::default()
    }
}

#[test]
fn an_archive_source_unpacks_and_is_pinned_by_its_digest() {
    if !curl_available() {
        return;
    }
    let root = scratch("archive");
    let archive = root.join("pkg-1.2.3.tar");
    write_release(&archive);
    let sha256 = digest(&archive);

    // The archive ships no `debian/`, which is the ordinary shape of a release
    // tarball, so its packaging comes from beside the recipe.
    let packaging = root.join("packaging");
    write_packaging(&packaging, "from packaging");
    let comp = overlaid_from_path(archive_component("pkg", &archive, &sha256), &packaging);
    let resolved = resolver_in(&root, &root).resolve(&comp).expect("resolve");

    // The tree is the archive's own root directory, so the release version does
    // not have to be written into the recipe a second time as a `subdir`.
    assert_eq!(resolved.tree, root.join("sources/pkg/pkg-1.2.3"));
    assert_eq!(resolved_marker(&resolved.tree), "from the archive\n");
    assert!(resolved.tree.join("src/main.c").is_file());
    assert_eq!(packaging_marker(&resolved.tree), "from packaging");

    // The digest pins the archive, so a build from one is as comparable as a
    // build from a revision.
    let source = &resolved.source.inputs()[0];
    assert_eq!(source.kind(), src2deb::SourceKind::Sha256);
    assert_eq!(source.role(), SourceRole::Source);
    assert_eq!(source.value(), sha256);
    assert!(resolved.source.is_pinned());
    assert_eq!(
        resolved.source.short(),
        format!(
            "{}.{}",
            &sha256[..7],
            &resolved.source.inputs()[1].value()[..7]
        ),
    );

    // It was cached under its digest, so a second component naming it fetches
    // nothing.
    assert!(root.join("tarballs").join(&sha256).is_file());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_pre_posix_v7_archive_unpacks_like_any_other() {
    if !curl_available() {
        return;
    }
    let root = scratch("archive-v7");
    let archive = root.join("pkg-1.2.3.tar");
    write_tar_v7(
        &archive,
        &[
            ("pkg-1.2.3/", ""),
            ("pkg-1.2.3/marker", "from the archive\n"),
            ("pkg-1.2.3/src/", ""),
            ("pkg-1.2.3/src/main.c", "int main(void) { return 0; }\n"),
        ],
    );
    // A v7 header carries no magic, so nothing in the file announces the format
    // and a reader that keys on the magic sees no tar archive at all.
    assert_eq!(&std::fs::read(&archive).unwrap()[257..265], &[0u8; 8]);

    let packaging = root.join("packaging");
    write_packaging(&packaging, "from packaging");
    let comp = overlaid_from_path(
        archive_component("pkg", &archive, &digest(&archive)),
        &packaging,
    );
    let resolved = resolver_in(&root, &root).resolve(&comp).expect("resolve");

    // The same tree a ustar archive of the same entries produces: the release
    // directory is descended into, the files are there, and the packaging
    // overlay lands on top.
    assert_eq!(resolved.tree, root.join("sources/pkg/pkg-1.2.3"));
    assert_eq!(resolved_marker(&resolved.tree), "from the archive\n");
    assert!(resolved.tree.join("src/main.c").is_file());
    assert_eq!(packaging_marker(&resolved.tree), "from packaging");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_archive_that_does_not_hash_to_what_was_declared_is_never_unpacked() {
    if !curl_available() {
        return;
    }
    let root = scratch("archive-mismatch");
    let archive = root.join("pkg-1.2.3.tar");
    write_release(&archive);

    let comp = archive_component("pkg", &archive, &"0".repeat(64));
    let err = resolver_in(&root, &root)
        .resolve(&comp)
        .expect_err("an archive that is not the declared one is not a source")
        .to_string();
    assert!(err.contains("Nothing was unpacked"), "{err}");
    assert!(
        !root.join("sources/pkg").exists(),
        "the archive was unpacked before its digest was checked",
    );
    // ...and nothing was left in the cache under a name it does not hash to.
    assert!(!root.join("tarballs").join("0".repeat(64)).exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_archive_that_cannot_be_fetched_fails_the_component() {
    if !curl_available() {
        return;
    }
    let root = scratch("archive-missing");
    let comp = archive_component("pkg", &root.join("absent.tar"), &"0".repeat(64));
    let err = resolver_in(&root, &root)
        .resolve(&comp)
        .expect_err("an archive that is not there is not a source")
        .to_string();
    assert!(err.contains("failed"), "{err}");
    assert!(!root.join("sources/pkg").exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_cached_archive_is_reused_and_re_verified() {
    if !curl_available() {
        return;
    }
    let root = scratch("archive-cache");
    let archive = root.join("pkg-1.2.3.tar");
    write_release(&archive);
    let sha256 = digest(&archive);
    let packaging = root.join("packaging");
    write_packaging(&packaging, "from packaging");
    let comp = overlaid_from_path(archive_component("pkg", &archive, &sha256), &packaging);

    let resolver = resolver_in(&root, &root);
    resolver.resolve(&comp).expect("first resolve");

    // The URL stops serving, and the run carries on from the cache: an archive
    // is identified by its digest, so what is already there is what was asked
    // for.
    std::fs::remove_file(&archive).unwrap();
    let again = resolver.resolve(&comp).expect("resolve from the cache");
    assert_eq!(resolved_marker(&again.tree), "from the archive\n");

    // ...but a cached archive that no longer hashes to its own name is not
    // trusted for having once been verified, and is cleared so a later run can
    // fetch it again.
    let cached = root.join("tarballs").join(&sha256);
    std::fs::write(&cached, "not the archive it was").unwrap();
    let err = resolver
        .resolve(&comp)
        .expect_err("a corrupted cache entry is not a source")
        .to_string();
    assert!(err.contains("Nothing was unpacked"), "{err}");
    assert!(
        !cached.exists(),
        "the corrupted archive was left in the cache"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_archive_is_unpacked_afresh_each_run() {
    if !curl_available() {
        return;
    }
    // What a patch series depends on: a patch applied twice over one tree does
    // not apply. A git source gets this from `git checkout --force` and a path
    // source from its fresh copy.
    let root = scratch("archive-afresh");
    let archive = root.join("pkg-1.2.3.tar");
    write_release(&archive);
    let sha256 = digest(&archive);
    let packaging = root.join("packaging");
    write_packaging(&packaging, "from packaging");
    let comp = overlaid_from_path(archive_component("pkg", &archive, &sha256), &packaging);

    let resolver = resolver_in(&root, &root);
    let first = resolver.resolve(&comp).expect("first resolve");
    std::fs::write(first.tree.join("vendor.tar"), "left by pass 1").unwrap();
    std::fs::write(first.tree.join("marker"), "edited in place\n").unwrap();

    let again = resolver.resolve(&comp).expect("second resolve");
    assert_eq!(resolved_marker(&again.tree), "from the archive\n");
    assert!(
        !again.tree.join("vendor.tar").exists(),
        "a prior run's output survived into a fresh unpack",
    );
    assert_eq!(again.source, first.source);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_subdir_descends_within_an_archives_own_root() {
    if !curl_available() {
        return;
    }
    // The two compose: the archive's root directory is found, and `subdir`
    // names a component within it — a member of a release that ships several.
    let root = scratch("archive-subdir");
    let archive = root.join("suite-1.0.tar");
    write_tar(
        &archive,
        &[
            ("suite-1.0/", ""),
            ("suite-1.0/members/", ""),
            ("suite-1.0/members/pkg/", ""),
            ("suite-1.0/members/pkg/marker", "the member\n"),
        ],
    );
    let sha256 = digest(&archive);
    let packaging = root.join("packaging");
    write_packaging(&packaging, "from packaging");

    let mut comp = overlaid_from_path(archive_component("pkg", &archive, &sha256), &packaging);
    comp.source.subdir = Some(PathBuf::from("members/pkg"));
    let resolved = resolver_in(&root, &root).resolve(&comp).expect("resolve");

    assert_eq!(
        resolved.tree,
        root.join("sources/pkg/suite-1.0/members/pkg")
    );
    assert_eq!(resolved_marker(&resolved.tree), "the member\n");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_packaging_overlay_may_come_from_an_archive() {
    if !curl_available() {
        return;
    }
    // A distribution publishes its packaging as a `.debian.tar.xz` beside the
    // orig tarball, so an overlay takes the same origins a source does.
    let root = scratch("archive-packaging");
    let upstream = root.join("working-tree");
    write_bare_tree(&upstream, "v1");
    let archive = root.join("pkg.debian.tar");
    write_tar(
        &archive,
        &[
            ("debian/", ""),
            ("debian/control", CONTROL),
            ("debian/marker", "from an archive\n"),
        ],
    );
    let sha256 = digest(&archive);

    let mut comp = path_component("pkg", &upstream);
    comp.packaging = Some(Source {
        tarball: Some(format!("file://{}", archive.display())),
        sha256: Some(sha256.clone()),
        ..Source::default()
    });
    let resolved = resolver_in(&root, &root).resolve(&comp).expect("resolve");

    assert_eq!(packaging_marker(&resolved.tree), "from an archive");
    let overlay = &resolved.source.inputs()[1];
    assert_eq!(overlay.role(), SourceRole::Packaging);
    assert_eq!(overlay.kind(), src2deb::SourceKind::Sha256);
    assert_eq!(overlay.value(), sha256);

    let _ = std::fs::remove_dir_all(&root);
}

/// Writes a `.dsc` at `path` declaring `format` over `files`, each named beside
/// it, and returns the digest a recipe would pin it by.
///
/// The files' own digests are measured rather than stated, so the `.dsc` a test
/// writes is one the resolver's own verification passes — which is what makes a
/// test that tampers with a file exercise the failure rather than a typo.
fn write_dsc(path: &Path, format: &str, files: &[&Path]) -> String {
    let mut text = format!("Format: {format}\nSource: pkg\nVersion: 1.2.3-4\n");
    text.push_str("Checksums-Sha256:\n");
    for file in files {
        let name = file.file_name().unwrap().to_string_lossy();
        let size = std::fs::metadata(file).unwrap().len();
        text.push_str(&format!(" {} {size} {name}\n", digest(file)));
    }
    std::fs::write(path, text).unwrap();
    digest(path)
}

/// Writes the two tarballs a `3.0 (quilt)` source package is assembled from: an
/// upstream release, and the `debian/` directory beside it.
fn write_quilt_files(root: &Path) -> (PathBuf, PathBuf) {
    let orig = root.join("pkg_1.2.3.orig.tar");
    write_tar(
        &orig,
        &[
            ("pkg-1.2.3/", ""),
            ("pkg-1.2.3/marker", "from the source package\n"),
            ("pkg-1.2.3/src/", ""),
            ("pkg-1.2.3/src/main.c", "int main(void) { return 0; }\n"),
        ],
    );
    let debian = root.join("pkg_1.2.3-4.debian.tar");
    write_tar(
        &debian,
        &[
            ("debian/", ""),
            ("debian/control", CONTROL),
            ("debian/marker", "from the source package's own packaging\n"),
            ("debian/patches/", ""),
            ("debian/patches/series", "fix.patch\n"),
        ],
    );
    (orig, debian)
}

/// A component built from the `.dsc` at `path`, pinned by `sha256`.
fn dsc_component(name: &str, path: &Path, sha256: &str) -> Component {
    Component {
        name: name.to_string(),
        source: Source {
            dsc: Some(format!("file://{}", path.display())),
            sha256: Some(sha256.to_string()),
            ..Source::default()
        },
        ..Component::default()
    }
}

#[test]
fn a_source_package_assembles_from_its_tarballs_and_needs_no_vendor_pass() {
    if !curl_available() {
        return;
    }
    let root = scratch("dsc");
    let (orig, debian) = write_quilt_files(&root);
    let dsc = root.join("pkg_1.2.3-4.dsc");
    let sha256 = write_dsc(&dsc, "3.0 (quilt)", &[&orig, &debian]);

    let resolved = resolver_in(&root, &root)
        .resolve(&dsc_component("pkg", &dsc, &sha256))
        .expect("resolve");

    // The tree is the upstream tarball's own root, with the packaging tarball
    // unpacked over it — which is the whole of what assembling a source package
    // amounts to, once the patch series is left to dpkg-buildpackage.
    assert_eq!(resolved.tree, root.join("sources/pkg/pkg-1.2.3"));
    assert_eq!(resolved_marker(&resolved.tree), "from the source package\n");
    assert!(resolved.tree.join("src/main.c").is_file());
    assert!(resolved.tree.join("debian/control").is_file());
    assert_eq!(
        packaging_marker(&resolved.tree),
        "from the source package's own packaging",
    );
    // The series travels unapplied. `dpkg-source --before-build` applies it
    // inside the cage, ahead of the build, so nothing on the host has to
    // understand quilt.
    assert!(resolved.tree.join("debian/patches/series").is_file());

    // One input, pinning the whole package: the `.dsc` carries the digest of
    // every file, so its own digest reaches all of them.
    assert_eq!(resolved.source.len(), 1);
    let source = &resolved.source.inputs()[0];
    assert_eq!(source.kind(), src2deb::SourceKind::Dsc);
    assert_eq!(source.role(), SourceRole::Source);
    assert_eq!(source.value(), sha256);
    assert!(resolved.source.is_pinned());
    assert_eq!(resolved.source.short(), sha256[..7]);

    // The claim the item is built on: a source package carries what its build
    // needs, so the one pass that reaches the host network is not run.
    assert_eq!(resolved.vendor, src2deb::VendorPass::Skip);

    // Every file it named is in the shared cache, under its own digest.
    for file in [&dsc, &orig, &debian] {
        assert!(
            root.join("tarballs").join(digest(file)).is_file(),
            "{} was not cached under its digest",
            file.display(),
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_clearsigned_source_package_is_read_through_its_armor() {
    if !curl_available() {
        return;
    }
    // What the archive actually publishes. The signature is not checked — the
    // recipe's declared digest is what says which file was meant — but the
    // armor still has to be read past.
    let root = scratch("dsc-signed");
    let (orig, debian) = write_quilt_files(&root);
    let dsc = root.join("pkg_1.2.3-4.dsc");
    write_dsc(&dsc, "3.0 (quilt)", &[&orig, &debian]);
    let body = std::fs::read_to_string(&dsc).unwrap();
    std::fs::write(
        &dsc,
        format!(
            "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\n{body}\
             -----BEGIN PGP SIGNATURE-----\n\nnot a real signature\n\
             -----END PGP SIGNATURE-----\n"
        ),
    )
    .unwrap();
    let sha256 = digest(&dsc);

    let resolved = resolver_in(&root, &root)
        .resolve(&dsc_component("pkg", &dsc, &sha256))
        .expect("resolve");
    assert_eq!(resolved_marker(&resolved.tree), "from the source package\n");
    // The digest pins the signed file, not the body inside it.
    assert_eq!(resolved.source.inputs()[0].value(), sha256);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_supplementary_upstream_tarball_lands_at_its_component_name() {
    if !curl_available() {
        return;
    }
    let root = scratch("dsc-components");
    let (orig, debian) = write_quilt_files(&root);
    // Two shapes are published: one wrapping its contents in a directory, one
    // laying them out flat. Both must arrive at the component's own name.
    let wrapped = root.join("pkg_1.2.3.orig-docs.tar");
    write_tar(
        &wrapped,
        &[
            ("docs-1.2.3/", ""),
            ("docs-1.2.3/manual.txt", "the manual\n"),
        ],
    );
    let flat = root.join("pkg_1.2.3.orig-test-data.tar");
    write_tar(&flat, &[("cases.txt", "one\n"), ("expected.txt", "two\n")]);
    let dsc = root.join("pkg_1.2.3-4.dsc");
    let sha256 = write_dsc(&dsc, "3.0 (quilt)", &[&orig, &wrapped, &flat, &debian]);

    let resolved = resolver_in(&root, &root)
        .resolve(&dsc_component("pkg", &dsc, &sha256))
        .expect("resolve");

    let tree = &resolved.tree;
    assert_eq!(
        std::fs::read_to_string(tree.join("docs/manual.txt")).unwrap(),
        "the manual\n",
    );
    assert_eq!(
        std::fs::read_to_string(tree.join("test-data/cases.txt")).unwrap(),
        "one\n",
    );
    // The staging directory each was unpacked through is not part of the source.
    let leftovers: Vec<PathBuf> = std::fs::read_dir(root.join("sources/pkg"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path != tree)
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_native_source_package_is_the_one_tarball_it_names() {
    if !curl_available() {
        return;
    }
    let root = scratch("dsc-native");
    let tarball = root.join("pkg_1.2.3.tar");
    write_tar(
        &tarball,
        &[
            ("pkg-1.2.3/", ""),
            ("pkg-1.2.3/marker", "native\n"),
            ("pkg-1.2.3/debian/", ""),
            ("pkg-1.2.3/debian/control", CONTROL),
            ("pkg-1.2.3/debian/marker", "native packaging\n"),
        ],
    );
    for format in ["3.0 (native)", "1.0"] {
        let dsc = root.join("pkg_1.2.3.dsc");
        let sha256 = write_dsc(&dsc, format, &[&tarball]);
        let resolved = resolver_in(&root, &root)
            .resolve(&dsc_component("pkg", &dsc, &sha256))
            .expect("resolve");
        assert_eq!(resolved_marker(&resolved.tree), "native\n", "{format}");
        assert_eq!(packaging_marker(&resolved.tree), "native packaging");
        assert_eq!(resolved.vendor, src2deb::VendorPass::Skip);
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_source_package_used_as_packaging_fetches_only_what_carries_debian() {
    if !curl_available() {
        return;
    }
    // "Build upstream's git with Debian's packaging". Only the packaging
    // tarball can carry a `debian/`, so only it is fetched — proved by naming
    // an upstream tarball that is not there at all, which a resolve that
    // reached for it could not pass.
    let root = scratch("dsc-packaging");
    let (orig, debian) = write_quilt_files(&root);
    let dsc = root.join("pkg_1.2.3-4.dsc");
    let sha256 = write_dsc(&dsc, "3.0 (quilt)", &[&orig, &debian]);
    std::fs::remove_file(&orig).unwrap();

    let upstream = root.join("working-tree");
    write_bare_tree(&upstream, "v1");
    let mut comp = path_component("pkg", &upstream);
    comp.packaging = Some(Source {
        dsc: Some(format!("file://{}", dsc.display())),
        sha256: Some(sha256.clone()),
        ..Source::default()
    });
    let resolved = resolver_in(&root, &root).resolve(&comp).expect("resolve");

    // The packaging came from the source package; the source did not.
    assert_eq!(
        packaging_marker(&resolved.tree),
        "from the source package's own packaging",
    );
    assert_eq!(resolved_marker(&resolved.tree), "v1");
    let overlay = &resolved.source.inputs()[1];
    assert_eq!(overlay.role(), SourceRole::Packaging);
    assert_eq!(overlay.kind(), src2deb::SourceKind::Dsc);
    assert_eq!(overlay.value(), sha256);
    // A packaging overlay says nothing about how the tree it is applied to
    // acquires its dependencies, so the vendor pass stays where it was.
    assert_eq!(resolved.vendor, src2deb::VendorPass::Run);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_component_file_that_does_not_match_the_dsc_fails_before_anything_is_unpacked() {
    if !curl_available() {
        return;
    }
    // The `.dsc` extends the recipe's one digest to every file it names, so a
    // mirror serving a different upstream tarball is caught even though the
    // `.dsc` itself is exactly the one the recipe pinned.
    let root = scratch("dsc-tampered");
    let (orig, debian) = write_quilt_files(&root);
    let dsc = root.join("pkg_1.2.3-4.dsc");
    let sha256 = write_dsc(&dsc, "3.0 (quilt)", &[&orig, &debian]);
    let declared = digest(&orig);
    write_tar(
        &orig,
        &[("pkg-1.2.3/", ""), ("pkg-1.2.3/marker", "swapped\n")],
    );

    let err = resolver_in(&root, &root)
        .resolve(&dsc_component("pkg", &dsc, &sha256))
        .expect_err("a file that does not match its declared digest fails")
        .to_string();
    assert!(err.contains(&declared), "{err}");
    assert!(err.contains("Nothing was unpacked"), "{err}");
    // The `.dsc` stays cached — it was the file that verified — and the
    // tarball that did not is removed, so a later run fetches it again.
    assert!(root.join("tarballs").join(&sha256).is_file());
    assert!(!root.join("tarballs").join(&declared).exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_source_package_src2deb_cannot_assemble_is_refused_with_the_alternative() {
    if !curl_available() {
        return;
    }
    // The one format that exists in practice and is not built: `1.0` with a
    // patch file, which is a second patch mechanism for a fraction of a percent
    // of the archive.
    let root = scratch("dsc-diff");
    let (orig, _) = write_quilt_files(&root);
    let diff = root.join("pkg_1.2.3-4.diff.gz");
    std::fs::write(&diff, b"not really gzip").unwrap();
    let dsc = root.join("pkg_1.2.3-4.dsc");
    let sha256 = write_dsc(&dsc, "1.0", &[&orig, &diff]);

    let err = resolver_in(&root, &root)
        .resolve(&dsc_component("pkg", &dsc, &sha256))
        .expect_err("a .diff.gz is not applied")
        .to_string();
    assert!(err.contains("diff.gz"), "{err}");
    assert!(err.contains("packaging overlay"), "{err}");
    // It failed on reading the `.dsc`, so nothing else was even fetched.
    assert!(!root.join("tarballs").join(digest(&orig)).exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_source_package_is_unpacked_afresh_each_run() {
    if !curl_available() {
        return;
    }
    // The guarantee a git checkout gets from `checkout --force` and a path
    // source from its fresh copy: a file the package does not carry does not
    // survive into the next run's tree.
    let root = scratch("dsc-afresh");
    let (orig, debian) = write_quilt_files(&root);
    let dsc = root.join("pkg_1.2.3-4.dsc");
    let sha256 = write_dsc(&dsc, "3.0 (quilt)", &[&orig, &debian]);
    let comp = dsc_component("pkg", &dsc, &sha256);

    let first = resolver_in(&root, &root).resolve(&comp).expect("resolve");
    std::fs::write(first.tree.join("stray"), "left behind").unwrap();

    let again = resolver_in(&root, &root).resolve(&comp).expect("resolve");
    assert_eq!(again.tree, first.tree);
    assert!(!again.tree.join("stray").exists(), "the stray survived");
    // The second resolve took every file from the cache, so it needed no
    // network — which is what makes a re-run cheap.
    assert!(root.join("tarballs").join(&sha256).is_file());

    let _ = std::fs::remove_dir_all(&root);
}
