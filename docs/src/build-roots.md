# Build roots

Every component builds in its own root filesystem, provisioned with the base
system, the toolchain, and that component's build-dependencies. src2deb chooses
one of two strategies per run, and caches a root it can safely reuse.

## Layered provisioning

On a host that supports an unprivileged overlay, src2deb bootstraps a shared base
once — the base system, the generic build toolchain, and the recipe's pinned Rust
toolchain if it names one — and then, for each component, installs only the
packages that component adds into a disposable overlay upper. The build cage
roots on an overlay of the shared base plus that increment.

One heavy bootstrap serves every component, and the base is never written by a
build: each component's upper is disposed when the component finishes, leaving
the base pristine for the next. The upper is staged fresh for every build rather
than reused, because the build writes into it through the overlay; reusing it
would carry one build's changes into the next. This is the default strategy.

A run that never gets to unwind — killed outright, or stopped with the second
Ctrl-C — leaves its upper and the overlay work directory beside it behind. The
next run clears both before staging, so a component recovers on its own rather
than inheriting a half-built layer.

## Full reprovisioning

Where the host cannot establish an unprivileged overlay, src2deb bakes a
fully-configured root filesystem per component instead. This is the fallback: the
build writes directly into the root, so a reused root carries the previous
build's changes — the weaker of the two isolation guarantees.

A run that falls back says so, at every verbosity above `-q`, naming what blocked
the overlay:

```text
src2deb: note: no unprivileged overlay (<reason>); using full reprovisioning,
         which reuses a root a build has written to
```

It reports by default because it changes what the run guarantees. The layered
strategy, being both the default and the stronger one, is announced under `-v`.

## The build-root cache

A root that a build does not mutate is cached on its resolved plan. Before
provisioning, src2deb asks the provisioner for the exact, archive-verified set of
packages a bootstrap would install, and records a key derived from that set —
each package's name, version, and archive checksum — beside the root. A later run
reuses the root only when the key still matches, and rebuilds it from clean when
the set has changed, so a bumped or added build-dependency never silently reuses
a root provisioned for the old set. This keys full reprovisioning's per-component
roots and layered provisioning's shared base.

The recipe's pinned Rust toolchain version is part of the key too, because the
toolchain is installed into the root as part of provisioning it. Without it the
key would describe less than the root holds, and repinning a recipe's toolchain
would reuse a root carrying the version it replaced.

The key names the suite and the architecture as well, so a root provisioned for
one target never matches another's key. Roots are therefore kept per target on
disk — `base/<suite>/<arch>/`, and likewise for `roots/` and `uppers/` — so a
work directory building for two architectures keeps a warm base for each rather
than each run rebuilding over the last one's. See
[Sharing one work directory](work-directory.md#sharing-one-work-directory).

Every run resolves that plan, including one that goes on to reuse the root
untouched: the archive has to be consulted to tell a current root from a stale
one. That is why a run reports fetching a release and an index even when it
installs nothing.

A run with nothing to build resolves nothing. The skip decision is made before
any root is provisioned, so a `--skip-published` re-run over unchanged sources
neither bootstraps the base nor consults the archive.

## Stopping mid-provision

A bootstrap — the shared base, or a fully-reprovisioned root — is stopped at the
next package boundary when a run is cancelled, and the partly-made root is
removed rather than left behind. A layered increment cannot be stopped once it
starts; it is small, so it runs to completion, and a cancelled run declines to
start another. See [Cancelling a run](how-a-build-runs.md#cancelling-a-run).
