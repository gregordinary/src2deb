# Checking installability

A run that goes twenty-six for twenty-six says every component **built**. It
says nothing about whether the packages it produced can be **installed**. Those
are different questions, and a package answers the first while failing the
second whenever a runtime `Depends` names something the target suite does not
have — a package that only exists in Ubuntu, one that was transitional in the
last release and is gone from this one, or one that is simply not packaged yet.

`src2deb check` asks the second question:

```sh
src2deb check recipes/cosmic-epoch --work /mnt/build/work
```

```text
src2deb: reading the archives for trixie/arm64 to check 27 package(s)
src2deb: arm64: cosmic-settings: Depends: network-manager-gnome
src2deb: arm64: cosmic-initial-setup-casper: Depends: casper
src2deb: arm64: 27 package(s), 214 dependencies, 2 unsatisfiable
src2deb: 2 unsatisfiable dependencies across 1 pool(s); apt will refuse those
packages until something provides what they name
```

It exits non-zero when anything is unsatisfiable, so it belongs between a build
and a publish.

## What it reads

The pool, not the recipe. That matters, because `debian/control` declares
`${shlibs:Depends}` while the `.deb` carries what that expanded to — every
library package the build actually linked against. The pool's index holds each
package's control stanza verbatim, so a check sees the dependencies a client
will see rather than the ones the packaging was written with.

It follows that a check covers whatever the pool holds, whichever recipe built
it. A work directory shared by three recipes checks as one archive, the same way
[pruning](using-the-pool.md#pruning-the-pool) does, because a pool for a suite
and architecture *is* one archive.

Every pool the suite holds is checked. `--architecture` narrows that, and is
repeatable:

```sh
src2deb check recipes/cosmic-epoch --architecture arm64
```

## What counts as available

The same archives a build root is provisioned from: the target suite, any
[additional repositories](recipes.md) the recipe declares, and the pool itself.
A dependency is satisfiable when one of those offers a package that satisfies
it.

Two consequences worth knowing:

- **A dependency the pool satisfies itself is fine.** The packages a recipe
  builds resolve against each other, so a metapackage depending on everything it
  pulls in checks clean once those are built.
- **A dependency satisfied only by a declared extra repository checks clean.** A
  recipe declaring a repository is declaring where its packages come from, and
  the check takes it at its word. A client that does not have that repository
  configured still cannot install the package.

Alternatives and virtual packages are honoured, over exactly the archives that
provision build roots. `a | b` is satisfied by either; `x-terminal-emulator` is
satisfied by anything that `Provides` it.

A dependency satisfied only by a provider is reported at `--verbose`, because it
is a weaker satisfaction than a direct one — the clause installs because apt
picks one of several providers, and which it picks is apt's decision rather than
the packaging's:

```text
src2deb: arm64: pop-icon-theme: Depends: adwaita-icon-theme-full is virtual,
provided by adwaita-icon-theme
```

## What is checked

`Depends` and `Pre-Depends` — the relationships that make a package
installable.

`Recommends` is not checked. apt passes over a `Recommends` it cannot satisfy
rather than refusing the package, so it does not belong in an answer about
installability.

Names are checked, not versions. A dependency's version constraint is not
enforced, for the same reason [build roots](build-roots.md) do not enforce one:
a suite is internally consistent, and the version a package resolves to in it is
the version that suite ships. What this catches is a dependency on a package
that is not there at all, which is the failure that reaches a target machine.

## After a build

A build ends with the same check over the pools it published into — every
architecture that built something, and no others:

```text
src2deb: 26 built, 0 failed, 1 skipped
src2deb: reading the archives for trixie/arm64 to check 27 package(s)
src2deb: arm64: 27 package(s), 214 dependencies, all satisfiable
src2deb: 1 pool(s) checked: 27 package(s), every dependency satisfiable
```

It is a note, not a gate: an unsatisfiable dependency does not fail the run. A
pool is often built before the packages that complete it — the recipe supplying
a dependency may not have run yet — so failing here would refuse a legitimate
order of work. `src2deb check` is where the same answer decides an exit status.

The note is skipped for a run that built nothing and for one that was cancelled.
A run that could not reach the archive to ask says so and leaves its own outcome
alone: failing to *ask* whether packages install is not a failure to build them.

It describes the pool as it stands, which is worth remembering after a
`--keep-going` run: a component that failed is a component whose packages are
not in the pool, so anything depending on them is reported alongside the failure
rather than instead of it.

## Acting on a finding

An unsatisfiable dependency has three ways out, and which applies is a property
of the package rather than of src2deb:

- **Build it.** The dependency is a package that could be built from source, and
  the answer is a recipe or a component for it. Four of the six COSMIC hit on
  Debian became one recipe of their own.
- **Drop it.** The dependency belongs to a platform this build is not for, and
  the answer is a patch to `debian/control`. `casper` is Ubuntu's live-boot
  system, reachable only through one binary package.
- **Point it elsewhere.** The dependency named a package that has been renamed or
  superseded, and something in the suite replaces it. `network-manager-gnome` is
  transitional in trixie and absent from forky, where `nm-connection-editor`
  takes over.

A dependency reported for one suite and not another is the ordinary case, not a
contradiction: suites move, and that is the whole reason the check reads the
suite the packages were built for.

## What it costs

One pass over the archives per pool: each one's release and package index,
fetched and projected down to the names it offers. A couple of seconds on a warm
link, for a pool of any size — the cost is the index, not the number of
dependencies asked about.

Nothing is resolved. A check computes no install closure and downloads no
package; it reads what the archives carry and answers each dependency against
that. So a pool for a foreign architecture is checked as readily as one for the
host's.

## In a scheduled build

```sh
#!/bin/sh
set -eu
src2deb build recipes/cosmic-epoch --work /mnt/build/work \
    --suite trixie --skip-published --keep 2
src2deb check recipes/cosmic-epoch --work /mnt/build/work --suite trixie
src2deb export recipes/cosmic-epoch --work /mnt/build/work \
    --suite trixie --to /srv/drop/rk1
```

`set -eu` makes the check the gate: a suite that moved underneath a package
stops the publish rather than reaching a target machine as an apt error. See
[Publishing to an archive](publishing.md).
