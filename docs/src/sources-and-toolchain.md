# Sources and the toolchain

Where packages and the compiler come from is declared in the recipe, along three
separate axes. The [recipe reference](recipes.md) lists the fields; this chapter
explains the model behind them.

The guiding constraint is that the underlying resolver is highest-version-wins
with no pin priorities. src2deb earns determinism by controlling the resolver's
inputs — the exact set of archives it sees — rather than by layering a
preferences engine on top.

## Archive sources

The primary suite and the feed-forward pool are always present. A recipe may add
named archives — a backports suite, a vendor archive, a local `file://` pool —
each with its own suite, mirror, and components. Every added archive is threaded
into provisioning for the shared base, each layer, and each full root.

A signed archive must name the keyring its release is verified against: the
provisioner has no embedded trust anchor for an archive other than the primary
Debian one. A local archive under your control may instead be trusted without a
signature.

## The toolchain

The Rust compiler and Cargo are selected separately from the archive list,
because a rustup toolchain is not a Debian archive:

- The **Debian** provider, the default, resolves `rustc` and `cargo` from the
  archive as ordinary build-dependencies. The build is only as new as the suite's
  Rust.
- The **rustup** provider installs a pinned toolchain into the build root and
  prefers it on `PATH`, while the archive's `rustc` and `cargo` stay installed to
  satisfy the component's declared build-dependencies. This decouples the
  compiler from the suite's Rust cadence — for example, building current COSMIC,
  which needs a newer `rustc` than Debian Trixie ships, on Trixie.

The rustup provider fetches the upstream installer from `https://sh.rustup.rs`
over pinned TLS (`--proto '=https' --tlsv1.2`) and installs the exact toolchain
version the recipe names. The installer script itself is not checksum-pinned —
this is the standard rustup bootstrap — so a rustup toolchain trusts that fetch
in addition to the archive. The Debian provider avoids it, resolving `rustc` and
`cargo` from the signed archive alone.

The install happens while a build root is being provisioned, not while a build is
running, so the toolchain is fetched once per root. Under the layered strategy
that means once per run for the whole recipe, rather than once for every
component: a build pass writes into a per-component overlay that is discarded
when the component finishes, so anything a pass installed would have to be
installed again for the next one. The pinned version is part of the root's cache
key, so repinning a recipe's toolchain provisions a fresh root rather than
reusing one holding the version it replaced. See
[Build roots](build-roots.md).
