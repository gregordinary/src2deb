# Command line

Two subcommands: `build` runs a recipe, `plan` resolves and orders one without
building. Both take a recipe directory and the same target options.

```sh
src2deb build RECIPE_DIR [options]
src2deb plan  RECIPE_DIR [options]
```

`RECIPE_DIR` is a directory containing a `recipe.toml`. Exactly one is required,
and it may appear before or after the options.

## Target options

Accepted by both subcommands. Each overrides the corresponding recipe field, so
one recipe serves every target it builds against.

| Option | Effect |
| --- | --- |
| `--work DIR` | The working directory for sources, build roots, the package cache, the pool, and output. Defaults to `./work` |
| `--suite SUITE` | Build for a Debian suite such as `trixie` or `forky`, superseding the recipe's `suite` and the `version-tag` that described it |
| `--architecture ARCH` | Build for a Debian architecture such as `amd64` or `arm64`. A recipe naming none builds for the host |
| `--arch-indep-owner ARCH` | Leave the recipe's `Architecture: all` packages to `ARCH`. Unset, every run produces its own |
| `--version-tag TAG` | Stamp built versions with `TAG`, such as `deb13`, overriding both the recipe's `version-tag` and the tag derived from the suite |

`--suite`, `--architecture`, `--arch-indep-owner`, and `--version-tag` are
validated as they are parsed, so a malformed value is a usage error against the
flag rather than a failure partway into the run. Each suite and architecture pair gets its own pool,
output tree, and manifest, so runs for several targets share one `--work`
directory. See [Cross-architecture builds](cross-architecture.md) and
[Package versions](package-versions.md).

## Build options

| Option | Effect |
| --- | --- |
| `--keep-going` | Build the remaining components after one fails and report a final tally. Covers a component whose source will not resolve as well as one whose build fails |
| `--jobs N` | Build up to `N` components concurrently, respecting the dependency order. Defaults to `1` |
| `--only C` | Build only component `C`. Repeatable |
| `--from C` | Build component `C` and every component after it in the build order |
| `--skip-published` | Skip a component whose source resolves to what a prior run recorded as built, at the same declared version. A source that is not pinned to exact content is always rebuilt |
| `--build-date DATE` | Stamp every version with `DATE` (`YYYY-MM-DD`) instead of today, and hand the build the same `SOURCE_DATE_EPOCH`. `--build-date manifest` takes the date the prior run recorded |

`--only` and `--from` are mutually exclusive, and `--jobs` takes an integer of 1
or more.

Both `--only` and `--from` narrow a run to part of its recipe, so whatever the
selected components build-depend on comes from the archive or from the pool. A
selection that leaves out a component producing one of those build-dependencies
is refused before anything is provisioned, naming the component to add. See
[What a narrowed run needs from the pool](quick-start.md#what-a-narrowed-run-needs-from-the-pool).

## Plan options

| Option | Effect |
| --- | --- |
| `--build-deps` | Print each component's build-dependencies alongside the order |

`plan` still clones every component's source, because the build order is read
from every `debian/control`. It takes the same exclusive lock on its work
directory that `build` does, so planning while a build runs wants a `--work`
directory of its own.

A component that [declares its version](recipes.md#components-with-no-changelog)
gets a line for it, which is where to see what `version-from = "git-describe"`
derived before a build stamps it into a package:

```text
  1. foo @ source 1f3a9c2e5b7d, packaging 8d4b0e1c7a92
     version: 1.2.3
```

## Verbosity

| Option | Prints |
| --- | --- |
| `-q`, `--quiet` | Failures, cancellation, and the closing summary |
| *(default)* | The progress narrative, the provisioning counters, the shared base's package count, any note that the run's guarantees changed, and each build's in-cage output |
| `-v`, `--verbose` | Adds per-component resolve and vendor detail, each root's own package count, and per-package provisioning detail |

Given both, the last on the command line wins. See
[Provisioning progress](how-a-build-runs.md#provisioning-progress).

## Information

| Option | Effect |
| --- | --- |
| `-h`, `--help` | Print usage and exit |
| `-V`, `--version` | Print the version and exit |

Either wins over the rest of the command line, so `src2deb build recipes/x
--help` prints usage.

## Streams

Progress, notes, and the closing summary go to standard error. The build order
`plan` produces goes to standard output, so it stays pipeable while a run
narrates alongside it:

```sh
src2deb plan recipes/cosmic-epoch | tail -5
```

In-cage build output is passed through to standard error, indented by two
spaces. Under `--jobs N` each line also carries its component's name. See
[In-cage build output](how-a-build-runs.md#in-cage-build-output).

## Exit status

| Status | Meaning |
| --- | --- |
| `0` | Every selected component built, or was skipped as already built |
| `1` | A component failed, or the run stopped before the build phase |
| `2` | A usage error: an unknown option, a malformed value, or a selection naming a component the recipe does not have |
| `130` | The run was cancelled with Ctrl-C or `SIGTERM` |

`130` outranks a component failure: a cancelled run did not finish, so nothing
follows about the components it never reached. The failure is still in the
summary and the manifest. See
[Cancelling a run](how-a-build-runs.md#cancelling-a-run).
