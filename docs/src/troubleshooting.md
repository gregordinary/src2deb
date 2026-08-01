# Troubleshooting

Every message src2deb prints is prefixed `src2deb:`. This chapter is ordered by
when a message appears in a run — before it starts, while sources resolve, while
the order is computed, while a build root is provisioned, and while a component
builds.

A run that reaches the build phase ends with a summary and exits non-zero if any
component failed. A run stopped before that point prints the error alone, writes
no manifest, and has nothing to summarize.

## Before the run starts

### `unrecognized option --x`

```text
src2deb: unrecognized option --nope
Try 'src2deb --help' for usage.
```

Exit status `2`. Each subcommand accepts its own options: `--build-deps` belongs
to `plan`, and `--keep-going`, `--jobs`, `--only`, `--from`, and
`--skip-published` to `build`. See [Command line](command-line.md).

### `the work directory is locked`

```text
src2deb: the work directory is locked by process 4127 (work/.lock); remove that
file if that process is gone
```

A run holds an exclusive lock on its work directory for its whole duration, so a
second run against the same `--work` is rejected rather than corrupting the
shared pool and output. `plan` takes the same lock, because it writes to
`<work>/sources/`.

Give the second run a `--work` directory of its own, or wait for the first. When
the named process is gone — a run killed outright, or ended by a second Ctrl-C,
leaves the lockfile behind — remove the file the message names. A lockfile
holding nothing readable as a process id reports the second form, "locked by
another run".

### `suite "x" is not a numbered Debian release`

```text
src2deb: suite "sid" is not a numbered Debian release, so it has no known version
tag; pass --version-tag to name the tag builds for it should carry (for example
"debsid")
```

src2deb derives a version tag from the suite for each numbered Debian release,
and refuses a suite it has no tag for rather than guessing one. Name the tag:

```sh
src2deb build recipes/cosmic-epoch --suite sid --version-tag debsid
```

A recipe written for such a suite may declare `version-tag` instead, which
applies to that recipe's own `suite`. See
[Package versions](package-versions.md#the-version-tag).

### `invalid selection`

Three forms, all settled before a single source is cloned or a root provisioned.

A selection naming a component the recipe does not have:

```text
src2deb: invalid selection: --only names unknown component "cosmic-osdd"
```

Check the name against `recipe.toml`, where component names are free-form.

A selection that leaves out a producer of something it selected:

```text
src2deb: invalid selection: --only builds "cosmic-osd", which build-depends on
  "libcosmic-randr-dev"; that package is produced by component "cosmic-randr",
  which --only leaves out and the pool does not hold. Select "cosmic-randr" as
  well, or build it first
```

Add the named component to the selection, or build it into the pool first — a
pool that already holds the package satisfies the build-dependency, which is why
this is refused only when the pool cannot cover it.

And a `--from` whose own source failed:

```text
src2deb: invalid selection: --from names component "cosmic-osd", whose source did
not resolve, so the components after it in the build order cannot be identified
```

`--from` names a position in the build order, and a component with no resolved
source has none. Fix the source, or name a different starting component.

## While sources resolve

### `resolving source for X`

```text
src2deb: FAILED cosmic-comp: resolving source for cosmic-comp: <reason>
```

A clone, fetch, checkout, or submodule update failed. Common causes are an
unreachable repository, a `git-ref` naming a branch or tag that does not exist,
and credentials for a private repository.

A resolve failure is a failure of that component rather than of the run, on the
same terms as a failed build: `--keep-going` carries the run past it, records the
component as failed with an empty commit, and folds it into the summary and the
manifest.

### `is stored with Git LFS, but git lfs is not available`

```text
src2deb: FAILED pop-icon-theme: resolving source for pop-icon-theme:
src/assets/wallpaper.png is stored with Git LFS, but `git lfs` is not available;
install git-lfs so the real content is fetched instead of the pointer stubs
standing in for it. Building against a pointer produces a package that installs
cleanly and fails at runtime, so the build stops here
```

Install `git-lfs` on the host:

```sh
sudo apt install git-lfs
```

A checkout made without LFS support writes a short text pointer where each asset
should be. Those pointers are ordinary valid files, so a build embeds or installs
one and succeeds — the substitution surfaces only when the installed program
reads the asset and finds a stub. src2deb therefore stops at resolve instead.

### `is still a Git LFS pointer after git lfs pull`

```text
src2deb: FAILED pop-icon-theme: resolving source for pop-icon-theme:
src/assets/wallpaper.png is still a Git LFS pointer after `git lfs pull`; the
content could not be fetched from the LFS server. Building against a pointer
produces a package that installs cleanly and fails at runtime, so the build stops
here
```

`git-lfs` is installed and ran, and the content is still missing. The LFS server
was unreachable, the objects it should hold are gone, or the repository needs
credentials for LFS that the clone did not have. Both messages list up to five
pointer paths, then a count of the rest.

### `skipping X (not selected); its source did not resolve`

```text
src2deb: skipping cosmic-player (not selected); its source did not resolve: <reason>
```

Reported, then passed over, whatever `--keep-going` says. Every run resolves
every component's source because the build order is read from all of them, so a
narrowed run still clones the whole recipe — but a component the run was never
going to build does not fail it. The recipe still has a problem the next full run
will hit.

## While the order is computed

### `cannot order the build: a dependency cycle`

```text
src2deb: cannot order the build: a dependency cycle involves: cosmic-osd, cosmic-randr
```

Two or more components each build-depend on a package another produces, so no
order satisfies them all. This ends the run outright: there is nothing coherent
to build. Break the cycle in the components' `debian/control` files.

### `reading debian/control for X`

```text
src2deb: FAILED cosmic-comp: reading debian/control for cosmic-comp: <reason>
```

The resolved tree has no `debian/control`, or it will not parse. Check the
component's `source.subdir` — a component inside a superproject needs it to point
at the directory holding the `debian/` tree. `debian/changelog` reports the same
way.

## While a build root is provisioned

### `provisioning a build root`

```text
src2deb: FAILED cosmic-comp: provisioning a build root: <reason>
```

Most often a build-dependency the target suite's archive cannot satisfy, which is
what an unsuitable `--suite` surfaces as: the flag retargets a recipe without
promising the recipe suits the target. Check that the component's
`Build-Depends` exist in the suite you asked for, and add a `[[repositories]]`
entry for anything that lives elsewhere. See
[Sources and the toolchain](sources-and-toolchain.md).

A foreign-architecture target with no `qemu-user` binfmt handler also fails here,
naming the missing handler. See [Requirements](cross-architecture.md#requirements).

### `installing the rustup X toolchain into a build root`

```text
src2deb: FAILED cosmic-comp: installing the rustup 1.95.0 toolchain into a build
root: <what the installer wrote>
```

The recipe pins a rustup toolchain and the install failed. The installer's output
is captured rather than streamed, so the message carries it. Common causes are a
version rustup does not publish for the target architecture and a host that
cannot reach `https://sh.rustup.rs`.

## While a component builds

### `vendoring X: debian/rules clean`

```text
src2deb: FAILED cosmic-comp: vendoring cosmic-comp: debian/rules clean exited with status 2
```

The vendor pass failed. It runs with the host's network so the component can
fetch its crates, so this is usually a network problem or an upstream vendoring
step that needs a tool the recipe has yet to declare. `extra-build-deps` adds one:

```toml
extra-build-deps = ["just"]
```

Re-run with `-v` to see the pass announced, and read the in-cage output above the
failure for what the vendoring step itself reported.

### `building X: dpkg-buildpackage`

```text
src2deb: FAILED cosmic-comp: building cosmic-comp: dpkg-buildpackage exited with status 2
```

An ordinary build failure. The build's own output is above the message, indented
by two spaces. The build pass runs offline from the `vendor.tar` the vendor pass
produced, so a build reporting a failed download means the vendoring step missed
something.

Both streams of the build render alike, because `dpkg-buildpackage` writes its
ordinary progress to standard error. A line's stream says nothing about its
severity.

## Notes that are not failures

Two things report at every verbosity above `-q`, because they change what a run
guarantees rather than what it is doing.

### `no unprivileged overlay`

```text
src2deb: note: no unprivileged overlay (<reason>); using full reprovisioning,
         which reuses a root a build has written to
```

The host cannot establish an unprivileged overlay, so src2deb bakes a full root
per component instead of layering each component's build-dependencies over one
shared base. Builds still work. They are slower, and a reused root carries the
previous build's writes, which is the weaker of the two isolation guarantees. See
[Build roots](build-roots.md).

### `foreign-architecture build`

```text
src2deb: foreign-architecture build: target arm64, host amd64 (runs through
qemu-user; needs qemu-user-static and binfmt with the F flag)
```

Every compiler invocation runs under emulation, which costs roughly an order of
magnitude in compile time. Expected when you asked for a foreign target;
otherwise check `--architecture` and the recipe's `architecture` field. See
[Cross-architecture builds](cross-architecture.md).

## Stopping and resuming

### `cancelled; stopping`

Ctrl-C stops a run at the next point where stopping leaves a coherent state
behind, so it can take until the current package, increment, or configure step
finishes. The run exits `130`. A second Ctrl-C exits immediately and leaves the
lockfile behind.

Components that finished stay built, published, and recorded. Re-run with
`--skip-published` to pick up where it stopped. See
[What a cancelled run leaves behind](how-a-build-runs.md#what-a-cancelled-run-leaves-behind).

### A re-run rebuilds everything

`--skip-published` reads the manifest for the recipe, suite, and architecture the
run targets, and skips a component whose source resolves to the commit recorded
as built. A run that rebuilds everything is reading a different manifest or a
different commit: check that `--work`, `--suite`, and `--architecture` match the
earlier run, and that the component's `git-ref` is a pinned commit rather than a
branch that has since moved.

### A build root is rebuilt when nothing changed

A root is cached on the exact set of packages a bootstrap would install — each
one's name, version, and archive checksum — plus the recipe's pinned toolchain
version. Any of those moving in the archive rebuilds the root from clean, which
is what keeps a bumped build-dependency from silently reusing a root provisioned
for the old set. See
[The build-root cache](build-roots.md#the-build-root-cache).
