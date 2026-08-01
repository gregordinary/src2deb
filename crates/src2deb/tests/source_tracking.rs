//! Git source-resolution behavior that a plain local checkout would get wrong:
//! a branch or default ref must advance to the fetched upstream tip on a re-run,
//! while a pinned commit must stay put. These drive real `git` against local
//! repositories, so they are skipped when `git` is unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use src2deb::Source;
use src2deb::recipe::Component;
use src2deb::source::SourceResolver;

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
    std::fs::write(
        upstream.join("debian/control"),
        "Source: pkg\n\nPackage: pkg\nArchitecture: any\n",
    )
    .unwrap();
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
            git: git.to_string_lossy().into_owned(),
            git_ref: git_ref.map(str::to_string),
            subdir: None,
        },
        extra_build_deps: Vec::new(),
    }
}

/// The marker content in a resolved tree.
fn resolved_marker(tree: &Path) -> String {
    std::fs::read_to_string(tree.join("marker")).unwrap()
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

    let resolver = SourceResolver::new(root.join("sources"));
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

    let resolver = SourceResolver::new(root.join("sources"));
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

    let resolver = SourceResolver::new(root.join("sources"));
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

    let resolver = SourceResolver::new(root.join("sources"));
    let comp = component("pkg", &upstream, Some(&pinned));

    // The resolved commit is the exact HEAD the tree was checked out at, which
    // the provenance manifest records.
    let resolved = resolver.resolve(&comp).expect("resolve");
    assert_eq!(resolved.commit, pinned);

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

    let resolver = SourceResolver::new(root.join("sources"));
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

    let resolver = SourceResolver::new(root.join("sources"));
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
