# Security

## Reporting a vulnerability

Report security issues through GitHub's private vulnerability reporting:
[open a draft advisory][report]. That keeps the report private until a fix is
available.

[report]: https://github.com/gregordinary/src2deb/security/advisories/new

Please include what you observed, the recipe and host that produced it, and the
src2deb version (`src2deb --version`). Expect an acknowledgement within a week.

Issues in the sandbox itself belong to
[ferroday-cage](https://github.com/gregordinary/ferroday-cage), which src2deb
builds on.

## What src2deb assumes

src2deb builds upstream source. Three properties of that are design decisions
rather than defects, and each is documented in the guide.

### The vendor pass runs upstream code with host network access

A component that vendors its dependencies is built in two passes. The vendor
pass runs the component's own `debian/rules clean` — arbitrary upstream code —
in a sandbox whose filesystem is isolated but whose network is the host's, so
the vendoring step can fetch its crates. `/etc/resolv.conf` is bound read-only
into that sandbox, and it is the one host file a build sees.

The offline build pass, which is the one that produces the packages, runs with
an isolated network. Building a recipe therefore means trusting the components
it names to the extent of running their build scripts with host network access.
See [the trust boundary][boundary].

[boundary]: https://gregordinary.github.io/src2deb/introduction.html#the-vendor-pass-is-the-trust-boundary

### Pools src2deb writes are unsigned

A run writes a `Release` and signs nothing, so a client reads the pool with
`Trusted: yes` — which installs packages without verifying a signature over the
archive. That is appropriate for a pool on the machine that built it, or one
served over a network you control to hosts you control. Sign the pool before
serving it to anyone else. See [Signing a pool][signing].

[signing]: https://gregordinary.github.io/src2deb/using-the-pool.html#signing-a-pool

### The rustup toolchain provider trusts an unpinned fetch

`provider = "rustup"` fetches the upstream installer from `https://sh.rustup.rs`
over pinned TLS and installs the version the recipe names. The installer script
carries no checksum pin, which is the standard rustup bootstrap, so that
toolchain rests on the fetch as well as on the archive. `provider = "debian"`
resolves `rustc` and `cargo` from the signed archive alone. See
[Sources and the toolchain][toolchain].

[toolchain]: https://gregordinary.github.io/src2deb/sources-and-toolchain.html#the-toolchain

## In scope

Reports that the sandbox fails to hold — a build reading or writing host state
outside the source tree, the output directory, and the work directory; a build
pass reaching the network; the host's environment, tooling, or keyring reaching
a build — are in scope, as are recipe or archive inputs that lead to code
execution outside a cage.

## Supported versions

src2deb is at 0.1. Fixes land on `main`; there are no maintained release
branches yet.
