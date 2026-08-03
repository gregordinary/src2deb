# src2deb

src2deb builds Debian `.deb` packages from source. Every component is built
inside an unprivileged [ferroday-cage][cage] sandbox, in a Debian root src2deb
provisions itself, and the finished packages are collected onto the host.

Its first target is the COSMIC desktop (cosmic-epoch): 27 components, built from
source for Debian Trixie and Forky using the `debian/` trees upstream ships.

[cage]: https://github.com/gregordinary/ferroday-cage

## Install

src2deb is built from source with Cargo. `rust-toolchain.toml` pins the Rust
version it builds with, which rustup installs on demand.

```sh
cargo install --path crates/src2deb-cli
```

That puts `src2deb` on `PATH` via `~/.cargo/bin`. To build without installing,
`cargo build --release` leaves the binary at `target/release/src2deb`.

The host needs Linux with unprivileged user namespaces, `git`, `git-lfs`, and
`curl`. src2deb provisions each build root itself, so the Debian build tooling
lives inside the sandbox.

## Usage

```sh
src2deb build recipes/cosmic-epoch --work ./work
```

src2deb resolves each component's source, derives the build order from every
`debian/control`, and builds the components in turn. Each one's packages are
published to a local pool that later components resolve against, so an
intra-recipe build-dependency is satisfied by the package src2deb just built.
What a run leaves behind is a servable Debian archive.

## Documentation

The [guide][docs] covers installing src2deb, running a build, writing recipes,
and serving the resulting pool to a machine that installs from it.

[docs]: https://gregordinary.github.io/src2deb/

## Layout

- `crates/src2deb` — the library: the build pipeline and the ferroday-cage seam.
- `crates/src2deb-cli` — the `src2deb` command.
- `recipes/` — build recipes, one directory each.
- `docs/` — the source of the guide.

## Status

src2deb is at 0.1, and its interfaces will change. The pipeline builds COSMIC
components from source end to end.

## License

MIT OR Apache-2.0.
