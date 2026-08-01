# Contributing

src2deb is at 0.1 and its interfaces will change. Issues and pull requests are
welcome.

## Building

`rust-toolchain.toml` pins the Rust version the project builds with, which
rustup installs on demand. From a checkout:

```sh
cargo build
cargo test --workspace
```

The unit tests build no packages and provision no roots, so they run anywhere
Cargo does. Exercising the pipeline end to end needs a host with unprivileged
user namespaces, `git`, and `git-lfs`, and a recipe to build:

```sh
cargo run -p src2deb-cli -- build recipes/pop-desktop-data --work ./work
```

`recipes/pop-desktop-data` is the cheapest of the three to run: five components
that build with plain `dh`, with no Rust toolchain to install and no vendoring
pass. Expect a first run to fetch several hundred packages for the shared base.

## What CI enforces

Every push and pull request runs the same four checks. Run them before opening a
pull request:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links" \
  cargo doc --no-deps -p src2deb
```

The workspace denies `unsafe_code` and warns on missing documentation.

## Documentation

Public items carry rustdoc comments: `///` for items, `//!` for modules and the
crate. The prose guide lives in `docs/` and is built with
[mdBook](https://rust-lang.github.io/mdBook/):

```sh
mdbook serve docs
```

A change to behaviour that a user would notice belongs in the guide as well as
in the doc comments. The guide describes what src2deb does, in the present
tense, and stays free of internal references.

## Commits

Write commit messages in the imperative mood, with a subject line that says what
changed and a body that says why. Group related changes into one commit rather
than splitting them across several.

## Recipes

A recipe is a directory under `recipes/` holding a `recipe.toml` and a README
covering what it builds, the upstream it builds from, and how to run it. See
[the recipe reference](docs/src/recipes.md).
