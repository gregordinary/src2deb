//! The src2deb command-line interface.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use src2deb::engine::Progress;
use src2deb::{
    Cancel, Engine, Fingerprint, PlanReport, Recipe, RunOptions, RunReport, Selection, SkipReason,
    SourceKind,
};

/// The exit status of a run stopped by Ctrl-C or `SIGTERM`.
///
/// 128 plus `SIGINT`, the shell's convention for a process ended by an
/// interrupt. src2deb handles the signal rather than dying from it, so the code
/// is a deliberate choice: it reports the same outcome a caller would see from
/// an unhandled Ctrl-C, and distinguishes a run the user stopped from one that
/// failed. It is reported for `SIGTERM` too, which is the same outcome.
const CANCELLED_EXIT: u8 = 130;

const USAGE: &str = "\
Build Debian packages from source in an unprivileged sandbox.

Usage:
  src2deb build RECIPE_DIR [--work DIR] [--suite SUITE] [--architecture ARCH]
                           [--arch-indep-owner ARCH] [--version-tag TAG]
                           [--keep-going] [--jobs N] [--only C]... | [--from C]
                           [--skip-published] [--build-date DATE|manifest]
  src2deb plan  RECIPE_DIR [--work DIR] [--suite SUITE] [--architecture ARCH]
                           [--arch-indep-owner ARCH] [--version-tag TAG]
                           [--build-deps]

Arguments:
  RECIPE_DIR            A directory containing a recipe.toml

Build options:
  --work DIR           Working directory for sources, roots, cache, pool, and
                       output (default: ./work)
  --keep-going         Build the remaining components after one fails, and
                       report a final tally, rather than stopping at the first
                       failure. Covers a component whose source will not
                       resolve as well as one whose build fails
  --jobs N             Build up to N components concurrently, respecting the
                       dependency order (default: 1)
  --only C             Build only component C (repeatable). Mutually exclusive
                       with --from
  --from C             Build component C and every component after it in the
                       build order
  --skip-published     Skip a component whose source is unchanged from what a
                       prior run recorded as built. A source that cannot be
                       pinned to exact content is always rebuilt
  --build-date DATE    Stamp every version with DATE (YYYY-MM-DD) instead of
                       today, and hand the build the same SOURCE_DATE_EPOCH, so
                       two runs from the same sources produce the same versions.
                       Pass \"manifest\" to take the date the prior run recorded,
                       which reproduces that build without transcribing it

  Both --only and --from narrow a run to part of its recipe, so whatever the
  components they select build-depend on has to come from the archive or from
  the pool. A selection that leaves out a component producing one of those
  build-dependencies is refused before anything is provisioned, naming the
  component to add.

Plan options:
  --work DIR           Working directory (sources are still cloned to read
                       debian/control). The plan takes the same exclusive lock
                       a build does, so use a separate --work to plan while a
                       build is running
  --build-deps         Also print each component's build-dependencies

Common options:
  --suite SUITE        Build for SUITE, a Debian suite name such as trixie or
                       forky, overriding the recipe's own suite. Each suite
                       gets its own pool, output tree, and manifest, so one
                       recipe serves every suite it builds against. The version
                       tag follows the suite, so a recipe's own version-tag is
                       superseded along with the suite it described
  --architecture ARCH  Build for ARCH, a Debian architecture name such as
                       amd64 or arm64, overriding whatever the recipe names.
                       A recipe that names none builds for the host, so one
                       recipe serves every target
  --arch-indep-owner ARCH
                       Leave the recipe's Architecture: all packages to ARCH.
                       Building for two architectures otherwise produces each
                       of them twice, under one name and version, which
                       collides when the architectures merge into one published
                       archive. Unset, every run produces its own, so a single
                       pool holds every package its recipe declares and can be
                       served as it stands
  --version-tag TAG    Stamp built versions with TAG (for example deb13),
                       overriding both the recipe's version-tag and the tag
                       derived from the suite. Required with a --suite that is
                       not a numbered Debian release
  -q, --quiet          Print only failures and the closing summary
  -v, --verbose        Print per-component detail and the in-cage build output
  -h, --help           Print this help
  -V, --version        Print the version
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match execute(&args) {
        Ok(code) => code,
        Err(Fault::Usage(message)) => {
            eprintln!("src2deb: {message}");
            eprintln!("Try 'src2deb --help' for usage.");
            ExitCode::from(2)
        }
        Err(Fault::Setup(message)) => {
            eprintln!("src2deb: {message}");
            ExitCode::FAILURE
        }
        // Cancelled before the run had anything to report — while sources were
        // resolving, or while the shared base bootstrapped. There is no summary
        // to print, only the outcome.
        Err(Fault::Run(src2deb::Error::Cancelled)) => {
            eprintln!("src2deb: cancelled");
            ExitCode::from(CANCELLED_EXIT)
        }
        Err(Fault::Run(err)) => {
            eprintln!("src2deb: {err}");
            ExitCode::FAILURE
        }
    }
}

/// A CLI failure: a usage error, a failure setting the process up, or an error
/// from the build itself.
#[derive(Debug)]
enum Fault {
    Usage(String),
    Setup(String),
    Run(src2deb::Error),
}

impl From<src2deb::Error> for Fault {
    fn from(err: src2deb::Error) -> Self {
        Fault::Run(err)
    }
}

/// A parsed command line.
#[derive(Debug, PartialEq, Eq)]
struct Cli {
    command: Command,
}

/// The subcommand to run, or an early-exit request.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// Print usage and exit successfully.
    Help,
    /// Print the version and exit successfully.
    Version,
    /// Build a recipe.
    Build(BuildArgs),
    /// Resolve and order a recipe without building.
    Plan(PlanArgs),
}

/// How much a run prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Verbosity {
    /// Only failures and the closing summary.
    Quiet,
    /// The progress narrative and in-cage build output.
    #[default]
    Normal,
    /// Adds per-component resolve/vendor detail and provisioning notes.
    Verbose,
}

/// The arguments to the `build` subcommand.
#[derive(Debug, PartialEq, Eq)]
struct BuildArgs {
    /// The directory holding `recipe.toml`.
    recipe_dir: PathBuf,
    /// The working directory for sources, roots, cache, pool, and output.
    work: PathBuf,
    /// The target suite, overriding the recipe's own when given.
    suite: Option<String>,
    /// The target architecture, overriding the recipe's own when given.
    architecture: Option<String>,
    /// The architecture that produces the recipe's `Architecture: all`
    /// packages, overriding the recipe's own when given.
    arch_indep_owner: Option<String>,
    /// The version tag to stamp, overriding both the recipe's own and the tag
    /// derived from the suite.
    version_tag: Option<String>,
    /// Keep building after a component fails.
    keep_going: bool,
    /// How many components to build concurrently (1 is sequential).
    jobs: usize,
    /// Which components to build.
    selection: Selection,
    /// Skip components a prior run already built from the same source.
    skip_published: bool,
    /// The date to stamp every version with.
    build_date: src2deb::BuildDate,
    /// How much to print.
    verbosity: Verbosity,
}

/// The arguments to the `plan` subcommand.
#[derive(Debug, PartialEq, Eq)]
struct PlanArgs {
    /// The directory holding `recipe.toml`.
    recipe_dir: PathBuf,
    /// The working directory (sources are still cloned to read `debian/control`).
    work: PathBuf,
    /// The target suite, overriding the recipe's own when given.
    suite: Option<String>,
    /// The target architecture, overriding the recipe's own when given.
    architecture: Option<String>,
    /// The architecture that produces the recipe's `Architecture: all`
    /// packages, overriding the recipe's own when given.
    arch_indep_owner: Option<String>,
    /// The version tag to stamp, overriding both the recipe's own and the tag
    /// derived from the suite.
    version_tag: Option<String>,
    /// Print each component's build-dependencies alongside the order.
    show_build_deps: bool,
    /// How much to print.
    verbosity: Verbosity,
}

/// Parses the command-line arguments into a [`Cli`], or returns a usage message.
///
/// `-h`/`--help` and `-V`/`--version` short-circuit even when other arguments
/// are present. Otherwise the first argument is the subcommand, and the rest are
/// parsed by that subcommand's parser.
fn parse_args(args: &[String]) -> Result<Cli, String> {
    // A help or version request anywhere on the line wins outright.
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => {
                return Ok(Cli {
                    command: Command::Help,
                });
            }
            "-V" | "--version" => {
                return Ok(Cli {
                    command: Command::Version,
                });
            }
            _ => {}
        }
    }

    let (command, rest) = args
        .split_first()
        .ok_or_else(|| "a subcommand is required: build, plan".to_string())?;
    let command = match command.as_str() {
        "build" => Command::Build(parse_build(rest)?),
        "plan" => Command::Plan(parse_plan(rest)?),
        other => return Err(format!("unknown subcommand {other}")),
    };
    Ok(Cli { command })
}

/// The `--work DIR` path, the `--suite SUITE`, `--architecture ARCH`, and
/// `--version-tag TAG` overrides, the single `RECIPE_DIR` positional, and the
/// verbosity shared by every subcommand.
struct CommonArgs {
    recipe_dir: PathBuf,
    work: PathBuf,
    suite: Option<String>,
    architecture: Option<String>,
    arch_indep_owner: Option<String>,
    version_tag: Option<String>,
    verbosity: Verbosity,
}

/// Collects the shared options (`--work DIR`, `--suite SUITE`,
/// `--architecture ARCH`, `--version-tag TAG`, `-q`/`-v`)
/// and the single `RECIPE_DIR` positional from a subcommand's arguments,
/// delegating any other option to `flag`. The callback receives the option and
/// the argument iterator — so it can consume a value — and returns whether it
/// recognized the option. When both `-q` and `-v` appear, the last one wins.
fn common_args(
    rest: &[String],
    mut flag: impl FnMut(&str, &mut std::slice::Iter<'_, String>) -> Result<bool, String>,
) -> Result<CommonArgs, String> {
    let mut positional: Vec<&str> = Vec::new();
    let mut work: Option<PathBuf> = None;
    let mut suite: Option<String> = None;
    let mut architecture: Option<String> = None;
    let mut arch_indep_owner: Option<String> = None;
    let mut version_tag: Option<String> = None;
    let mut verbosity = Verbosity::Normal;
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--work" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--work requires a value".to_string())?;
                work = Some(PathBuf::from(value));
            }
            "--suite" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--suite requires a value".to_string())?;
                // Checked here rather than at load, for the same reason as the
                // architecture: a malformed name is a usage error against the
                // flag, not an error against a recipe that is itself fine.
                if let Some(reason) = src2deb::recipe::suite_name_error(value) {
                    return Err(format!("--suite value {value:?} {reason}"));
                }
                suite = Some(value.clone());
            }
            "--architecture" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--architecture requires a value".to_string())?;
                // Checked here rather than at load, so a malformed name is a
                // usage error against the flag instead of an error against a
                // recipe that is itself fine.
                if let Some(reason) = src2deb::arch::architecture_name_error(value) {
                    return Err(format!("--architecture value {value:?} {reason}"));
                }
                architecture = Some(value.clone());
            }
            "--arch-indep-owner" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--arch-indep-owner requires a value".to_string())?;
                // Checked here for the same reason as --architecture: it is
                // compared against one, so a name that could never be one would
                // hand arch-indep output to nothing.
                if let Some(reason) = src2deb::arch::architecture_name_error(value) {
                    return Err(format!("--arch-indep-owner value {value:?} {reason}"));
                }
                arch_indep_owner = Some(value.clone());
            }
            "--version-tag" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--version-tag requires a value".to_string())?;
                // Checked here for the same reason as the suite and the
                // architecture: a tag the Debian version grammar refuses is a
                // usage error against the flag, and catching it now keeps it
                // from surfacing as an opaque build failure.
                if let Some(reason) = src2deb::recipe::version_tag_error(value) {
                    return Err(format!("--version-tag value {value:?} {reason}"));
                }
                version_tag = Some(value.clone());
            }
            "-q" | "--quiet" => verbosity = Verbosity::Quiet,
            "-v" | "--verbose" => verbosity = Verbosity::Verbose,
            other if other.starts_with('-') && other != "-" => {
                if !flag(other, &mut iter)? {
                    return Err(format!("unrecognized option {other}"));
                }
            }
            other => positional.push(other),
        }
    }
    let recipe_dir = match positional.as_slice() {
        [dir] => PathBuf::from(dir),
        [] => return Err("a RECIPE_DIR is required".to_string()),
        _ => return Err("takes exactly one RECIPE_DIR".to_string()),
    };
    Ok(CommonArgs {
        recipe_dir,
        work: work.unwrap_or_else(|| PathBuf::from("work")),
        suite,
        architecture,
        arch_indep_owner,
        version_tag,
        verbosity,
    })
}

/// Parses the `build` subcommand's arguments.
fn parse_build(rest: &[String]) -> Result<BuildArgs, String> {
    let mut keep_going = false;
    let mut jobs = 1usize;
    let mut only: Vec<String> = Vec::new();
    let mut from: Option<String> = None;
    let mut skip_published = false;
    let mut build_date: Option<src2deb::BuildDate> = None;
    let common = common_args(rest, |flag, iter| match flag {
        "--keep-going" => {
            keep_going = true;
            Ok(true)
        }
        "--jobs" => {
            let value = iter
                .next()
                .ok_or_else(|| "--jobs requires a value".to_string())?;
            jobs = value
                .parse()
                .map_err(|_| format!("--jobs value {value:?} is not a positive integer"))?;
            if jobs == 0 {
                return Err("--jobs must be at least 1".to_string());
            }
            Ok(true)
        }
        "--only" => {
            only.push(
                iter.next()
                    .ok_or_else(|| "--only requires a value".to_string())?
                    .clone(),
            );
            Ok(true)
        }
        "--from" => {
            from = Some(
                iter.next()
                    .ok_or_else(|| "--from requires a value".to_string())?
                    .clone(),
            );
            Ok(true)
        }
        "--skip-published" => {
            skip_published = true;
            Ok(true)
        }
        "--build-date" => {
            let value = iter
                .next()
                .ok_or_else(|| "--build-date requires a value".to_string())?;
            build_date = Some(parse_build_date(value)?);
            Ok(true)
        }
        _ => Ok(false),
    })?;

    let selection = match (only.is_empty(), from) {
        (false, Some(_)) => return Err("--only and --from are mutually exclusive".to_string()),
        (false, None) => Selection::Only(only),
        (true, Some(component)) => Selection::From(component),
        (true, None) => Selection::All,
    };

    Ok(BuildArgs {
        recipe_dir: common.recipe_dir,
        work: common.work,
        suite: common.suite,
        architecture: common.architecture,
        arch_indep_owner: common.arch_indep_owner,
        version_tag: common.version_tag,
        keep_going,
        jobs,
        build_date: build_date.unwrap_or_default(),
        selection,
        skip_published,
        verbosity: common.verbosity,
    })
}

/// Parses the `plan` subcommand's arguments.
fn parse_plan(rest: &[String]) -> Result<PlanArgs, String> {
    let mut show_build_deps = false;
    let common = common_args(rest, |flag, _iter| match flag {
        "--build-deps" => {
            show_build_deps = true;
            Ok(true)
        }
        _ => Ok(false),
    })?;
    Ok(PlanArgs {
        recipe_dir: common.recipe_dir,
        work: common.work,
        suite: common.suite,
        architecture: common.architecture,
        arch_indep_owner: common.arch_indep_owner,
        version_tag: common.version_tag,
        show_build_deps,
        verbosity: common.verbosity,
    })
}

/// Parses the arguments and dispatches to the selected command.
fn execute(args: &[String]) -> Result<ExitCode, Fault> {
    let cli = parse_args(args).map_err(Fault::Usage)?;
    match cli.command {
        Command::Help => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        Command::Version => {
            println!("src2deb {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        Command::Build(args) => build(args),
        Command::Plan(args) => plan(args),
    }
}

/// Loads the recipe from `dir`, retargeting it when `--suite`,
/// `--architecture`, or `--version-tag` named one.
///
/// Each flag wins over the recipe's own field, so one recipe builds for several
/// suites and architectures without being edited — and a recipe that names no
/// architecture stays portable, taking the host's by default. The recipe's own
/// values are the defaults, not a binding: what a recipe fixes is the components
/// and how they are built, not which target the run aims at. Every name was
/// checked when the arguments were parsed, so they are safe to install here.
///
/// `--suite` supersedes the recipe's `version-tag` along with its `suite`. The
/// tag identifies the suite a package was built for, and a recipe declares one
/// for the suite it declares — so carrying it onto another target would stamp
/// packages with the name of a release they were not built against, and defeat
/// the ordering the tag exists to guarantee. The new suite derives its own tag,
/// or `--version-tag` names one; anything else is refused rather than guessed.
fn load_recipe(
    dir: &Path,
    suite: Option<String>,
    architecture: Option<String>,
    arch_indep_owner: Option<String>,
    version_tag: Option<String>,
) -> Result<Recipe, Fault> {
    let mut recipe = Recipe::load(dir)?;
    if let Some(suite) = suite {
        recipe.suite = suite;
        recipe.version_tag = None;
    }
    if let Some(architecture) = architecture {
        recipe.architecture = architecture;
    }
    if let Some(owner) = arch_indep_owner {
        recipe.arch_indep_owner = Some(owner);
    }
    if let Some(version_tag) = version_tag {
        recipe.version_tag = Some(version_tag);
    }
    // The recipe validated its own suite against the version tags src2deb
    // knows; a suite that arrived afterwards has to clear the same bar. Only a
    // retargeted run can reach this, since a loaded recipe always resolves a
    // tag — but the message does not say so, so that it stays true whatever
    // reaches it. A usage error here names the flag that settles it, rather
    // than surfacing partway into the run as an error against a recipe that is
    // itself fine.
    if recipe.resolved_version_tag().is_none() {
        return Err(Fault::Usage(format!(
            "suite {:?} is not a numbered Debian release, so it has no known \
             version tag; pass --version-tag to name the tag builds for it should \
             carry (for example \"debsid\")",
            recipe.suite
        )));
    }
    Ok(recipe)
}

/// Prints the recipe banner shared by `build` and `plan`: name, suite,
/// architecture, component count, and the toolchain and extra repositories.
/// Suppressed when quiet.
fn print_recipe_banner(recipe: &Recipe, verb: &str, verbosity: Verbosity) {
    if verbosity == Verbosity::Quiet {
        return;
    }
    eprintln!(
        "src2deb: {verb} recipe '{}' ({}/{}) with {} component(s)",
        recipe.name,
        recipe.suite,
        recipe.architecture,
        recipe.components.len()
    );
    if let Some(version) = recipe.toolchain.rust.rustup_version() {
        eprintln!("src2deb: rust toolchain: rustup {version}");
    }
    if !recipe.repositories.is_empty() {
        let names: Vec<&str> = recipe
            .repositories
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        eprintln!(
            "src2deb: {} extra repositor{}: {}",
            recipe.repositories.len(),
            if recipe.repositories.len() == 1 {
                "y"
            } else {
                "ies"
            },
            names.join(", ")
        );
    }
}

/// Turns Ctrl-C and `SIGTERM` into a request the run consults, and returns the
/// signal to pass in.
///
/// The handler stores into an atomic and nothing else, which is all a handler
/// may safely do; the run reads it at its own boundaries. A second signal exits
/// the process immediately, so a graceful stop that is itself stuck is still
/// escapable — the build sandboxes are tied to this process's lifetime, so they
/// go down with it. The conditional shutdown registers first, so it sees the
/// flag as the *previous* signal left it rather than as this one sets it.
fn cancel_on_signal() -> Result<Cancel, Fault> {
    let cancel = Cancel::new();
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register_conditional_shutdown(
            signal,
            CANCELLED_EXIT.into(),
            cancel.flag(),
        )
        .and_then(|_| signal_hook::flag::register(signal, cancel.flag()))
        .map_err(|err| Fault::Setup(format!("installing the signal handler: {err}")))?;
    }
    Ok(cancel)
}

/// Runs the `build` subcommand, printing a closing summary and reporting an
/// unsuccessful exit if any component failed or the run was cancelled.
fn build(args: BuildArgs) -> Result<ExitCode, Fault> {
    let recipe = load_recipe(
        &args.recipe_dir,
        args.suite,
        args.architecture,
        args.arch_indep_owner,
        args.version_tag,
    )?;
    // A `--only` or `--from` naming a component the recipe does not have is a
    // usage error, and the recipe alone settles it. The engine checks it too, as
    // the authority for any caller; catching it here is what makes a typo exit
    // with the usage status rather than as a failed run.
    args.selection
        .validate(&recipe)
        .map_err(|err| Fault::Usage(err.to_string()))?;

    let options = RunOptions {
        keep_going: args.keep_going,
        jobs: args.jobs,
        build_date: args.build_date,
        selection: args.selection,
        skip_published: args.skip_published,
        cancel: cancel_on_signal()?,
    };
    let mut reporter = Reporter::new(args.verbosity, args.jobs);
    let mut engine = Engine::new(args.work);
    // Held rather than propagated, so the reporter finishes its last row before
    // the summary or the error lands on it.
    let outcome = engine.run(&recipe, &options, &mut |event| {
        // Announced on the run's own first event rather than before the call,
        // so a run the work-directory lock rejects is not first said to be
        // building. See `Progress::Started`.
        if let Progress::Started = event {
            print_recipe_banner(&recipe, "building", args.verbosity);
            if args.jobs > 1 && args.verbosity != Verbosity::Quiet {
                eprintln!(
                    "src2deb: building up to {} component(s) concurrently",
                    args.jobs
                );
            }
        }
        reporter.report(event);
    });
    reporter.finish();

    let report = outcome?;
    print_summary(&report);
    Ok(ExitCode::from(exit_status(&report)))
}

/// The exit status a finished run reports.
///
/// Cancellation outranks a failure inside the run: a cancelled run did not
/// finish, so nothing can be concluded about the components it never reached,
/// and that is the more important thing to tell a caller. The failure is still
/// in the summary and the manifest.
fn exit_status(report: &RunReport) -> u8 {
    if report.cancelled {
        CANCELLED_EXIT
    } else if report.is_success() {
        0
    } else {
        1
    }
}

/// Runs the `plan` subcommand: resolves sources, computes the build order, and
/// prints it without building.
fn plan(args: PlanArgs) -> Result<ExitCode, Fault> {
    let recipe = load_recipe(
        &args.recipe_dir,
        args.suite,
        args.architecture,
        args.arch_indep_owner,
        args.version_tag,
    )?;

    // Planning clones every component's source, which is slow enough to be
    // worth interrupting even though it builds nothing.
    let cancel = cancel_on_signal()?;
    let mut reporter = Reporter::planning(args.verbosity);
    let engine = Engine::new(args.work);
    let outcome = engine.plan(&recipe, &cancel, &mut |event| {
        if let Progress::Started = event {
            print_recipe_banner(&recipe, "planning", args.verbosity);
        }
        reporter.report(event);
    });
    reporter.finish();

    print_plan(&outcome?, args.show_build_deps);
    Ok(ExitCode::SUCCESS)
}

/// Which per-package phase of provisioning a counter is reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    /// Fetching a package into the shared cache.
    Download,
    /// Unpacking a package into the build root.
    Unpack,
}

impl Phase {
    /// The verb this phase is rendered with.
    fn verb(self) -> &'static str {
        match self {
            Phase::Download => "downloading",
            Phase::Unpack => "unpacking",
        }
    }
}

/// How a run renders the per-package download and unpack counters.
///
/// Provisioning reports one event per package, which at several hundred
/// packages a root is far too much to print verbatim unless it was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Counter {
    /// Nothing at all, when quiet.
    Silent,
    /// A single row rewritten in place. Only for a sequential run on a
    /// terminal: several workers rewriting one row is unreadable, and a
    /// redirected stream would collect carriage returns rather than lines.
    InPlace,
    /// One line each time the count passes a tenth of the total — the readable
    /// fallback for a concurrent run or a redirected stream.
    Periodic,
    /// One line per package, when verbose.
    PerPackage,
}

impl Counter {
    /// The style a run of `jobs` components at `verbosity` renders with, on a
    /// stderr that is or is not a `terminal`.
    fn of(verbosity: Verbosity, jobs: usize, terminal: bool) -> Counter {
        match verbosity {
            Verbosity::Quiet => Counter::Silent,
            Verbosity::Verbose => Counter::PerPackage,
            Verbosity::Normal if jobs <= 1 && terminal => Counter::InPlace,
            Verbosity::Normal => Counter::Periodic,
        }
    }
}

/// Renders a run's progress events to stderr.
///
/// `Quiet` shows only failures and cancellation (the closing summary is printed
/// separately). `Normal` adds the progress narrative, the provisioning
/// counters, the shared base's package count, and the in-cage build output.
/// `Verbose` adds per-component resolve/vendor detail, the layered-provisioning
/// note, and per-package provisioning detail.
///
/// The rule for what earns a place at the default verbosity is whether it
/// changes what the run *guarantees* or what it *costs*, rather than what it is
/// doing: the overlay fallback weakens the isolation between builds, and the
/// shared base's package count is the largest commitment a run makes, so both
/// report by default while the narrative of individual packages does not.
///
/// It holds state because the counter can be a row rewritten in place: the row
/// has to be blanked before any permanent line is printed over it, and the
/// periodic style has to remember how far each root has got.
struct Reporter {
    verbosity: Verbosity,
    /// Whether in-cage output lines carry a component label, since a concurrent
    /// build interleaves several components' output.
    labeled: bool,
    /// Whether the resolved build order is announced as it is computed. A
    /// `plan` prints the order itself, in a richer form and on stdout, so
    /// announcing it here would print it twice.
    announce_order: bool,
    /// How the provisioning counters render.
    counter: Counter,
    /// The width of the rewritten counter row on screen, or 0 when the cursor
    /// sits at the start of a clean row.
    row: usize,
    /// The last tenth reported for each root and phase, for the periodic style.
    tenths: BTreeMap<(String, Phase), usize>,
}

impl Reporter {
    /// Creates a reporter for a build of `jobs` components at `verbosity`.
    fn new(verbosity: Verbosity, jobs: usize) -> Reporter {
        Reporter {
            verbosity,
            labeled: jobs > 1,
            announce_order: true,
            counter: Counter::of(verbosity, jobs, std::io::stderr().is_terminal()),
            row: 0,
            tenths: BTreeMap::new(),
        }
    }

    /// Creates a reporter for a `plan` at `verbosity`, which resolves the build
    /// order and then prints it to stdout itself.
    fn planning(verbosity: Verbosity) -> Reporter {
        Reporter {
            announce_order: false,
            ..Reporter::new(verbosity, 1)
        }
    }

    /// Leaves the cursor on a clean row. Call once the run is over, so the
    /// closing summary or an error does not land on a half-written counter.
    fn finish(&mut self) {
        self.clear_row();
    }

    /// Prints one progress event.
    fn report(&mut self, event: Progress) {
        // The counter is the only row that is written over; every other line is
        // permanent and starts from a clean one.
        if !self.rewrites_the_row(&event) {
            self.clear_row();
        }

        // Failure and cancellation are always shown: they are why a run ends
        // where it does.
        match event {
            Progress::Failed { component, error } => {
                eprintln!("src2deb: FAILED {component}: {error}");
                return;
            }
            Progress::Cancelled => {
                eprintln!("src2deb: cancelled; stopping");
                return;
            }
            _ => {}
        }
        if self.verbosity == Verbosity::Quiet {
            return;
        }

        let verbose = self.verbosity == Verbosity::Verbose;
        match event {
            // Verbose-only per-component and provisioning detail.
            Progress::Resolving { component } if verbose => {
                eprintln!("src2deb: resolving {component}")
            }
            Progress::Vendoring { component } if verbose => {
                eprintln!("src2deb: vendoring {component} (network)")
            }
            Progress::Layered if verbose => {
                eprintln!("src2deb: layered provisioning (shared base + per-component overlay)")
            }
            Progress::Fetching { component, url } if verbose => {
                eprintln!("src2deb: {}: fetching {url}", root_label(component))
            }
            // Normal narrative.
            //
            // The fallback is the weaker of the two provisioning strategies —
            // it reuses a root a build has written to — so it reports whatever
            // the verbosity, while the layered default above does not. What a
            // run guarantees is not a detail.
            Progress::OverlayUnavailable { reason } => eprintln!(
                "src2deb: note: no unprivileged overlay ({reason}); using full \
                 reprovisioning, which reuses a root a build has written to"
            ),
            // The shared base is the largest single commitment a run makes, and
            // nothing else tells the user how large before it is under way. Each
            // component's own count stays behind `-v`, where it is one line
            // among many rather than the run's headline cost.
            Progress::PackagesResolved {
                component,
                packages,
            } if verbose || component.is_none() => eprintln!(
                "src2deb: {}: {packages} package(s) to install",
                root_label(component)
            ),
            // Not a failure of this run: the component was outside its
            // selection, so nothing was going to be built from that source. It
            // is still worth saying, because the recipe has a problem the next
            // full run will hit.
            Progress::Unresolved { component, error } => eprintln!(
                "src2deb: skipping {component} (not selected); its source did not \
                 resolve: {error}"
            ),
            Progress::ForeignArchitecture { target, host } => eprintln!(
                "src2deb: foreign-architecture build: target {target}, host {host} \
                 (runs through qemu-user; needs qemu-user-static and binfmt with the F flag)"
            ),
            // Which date a reproduction settled on, said up front rather than
            // left to be read off the versions once the packages exist.
            Progress::BuildDate { date } => {
                eprintln!("src2deb: stamping every version with build date {date}")
            }
            // A build here produces fewer packages than the recipe declares, and
            // its pool is not servable on its own. Said up front — by `plan` as
            // well as `build` — rather than left to be noticed in the tally of a
            // run that has already finished.
            Progress::ArchIndepElsewhere { owner } => eprintln!(
                "src2deb: Architecture: all packages belong to {owner}; this architecture \
                 builds only its own, so its pool holds fewer packages than the recipe \
                 declares"
            ),
            Progress::Ordered { order } if self.announce_order => {
                eprintln!("src2deb: build order: {}", order.join(" -> "));
            }
            Progress::Provisioning { component } => match component {
                None => eprintln!("src2deb: provisioning the shared base"),
                Some(component) => {
                    eprintln!("src2deb: provisioning the build root for {component}")
                }
            },
            Progress::InstallingToolchain { component, version } => eprintln!(
                "src2deb: {}: installing the rustup {version} toolchain",
                root_label(component)
            ),
            Progress::Downloading {
                component,
                package,
                index,
                total,
            } => self.count(component, Phase::Download, package, index, total),
            Progress::Extracting {
                component,
                package,
                index,
                total,
            } => self.count(component, Phase::Unpack, package, index, total),
            Progress::Building {
                component,
                index,
                total,
            } => eprintln!("src2deb: building {component} ({index}/{total})"),
            Progress::Built {
                component,
                artifacts,
            } => eprintln!("src2deb: built {component}: {artifacts} artifact(s)"),
            Progress::Published { component, debs } => {
                eprintln!("src2deb: published {debs} package(s) from {component} to the pool");
            }
            // In-cage output, indented to set it apart from src2deb's own
            // narrative and otherwise passed through as the build wrote it.
            //
            // Both streams render alike. Debian's build tooling does not use
            // the choice of stream to signal severity — `dpkg-buildpackage`
            // writes its ordinary progress to stderr — so marking one of them
            // would draw the eye to lines that mean nothing by it, in exactly
            // the long logs where a real problem has to stand out.
            Progress::Output {
                component, line, ..
            } => {
                if self.labeled {
                    eprintln!("  [{component}] {line}");
                } else {
                    eprintln!("  {line}");
                }
            }
            Progress::Skipped { component, reason } => {
                eprintln!("src2deb: skipping {component} ({reason})");
            }
            Progress::Manifest { path } => {
                eprintln!("src2deb: wrote provenance manifest to {}", path.display());
            }
            // Verbose-only variants at a lower verbosity, and any milestone
            // added later (Progress is #[non_exhaustive]).
            _ => {}
        }
    }

    /// Renders one package's download or unpack, in the run's counter style.
    fn count(
        &mut self,
        component: Option<&str>,
        phase: Phase,
        package: &str,
        index: usize,
        total: usize,
    ) {
        let root = root_label(component);
        let verb = phase.verb();
        match self.counter {
            Counter::Silent => {}
            Counter::PerPackage => {
                eprintln!("src2deb: {root}: {verb} {package} ({index}/{total})");
            }
            Counter::InPlace => {
                self.rewrite_row(&format!("src2deb: {root}: {verb} {index}/{total}"));
            }
            Counter::Periodic => {
                // A package the cache already holds reports nothing, so the
                // count arrives in jumps; reporting on a crossed tenth rather
                // than an exact one keeps a warm cache from going quiet.
                let tenth = index * 10 / total.max(1);
                let reached = self.tenths.entry((root.to_string(), phase)).or_default();
                if tenth > *reached {
                    *reached = tenth;
                    eprintln!("src2deb: {root}: {verb} {index}/{total}");
                }
            }
        }
    }

    /// Whether this event writes over the counter row rather than printing a
    /// permanent line below it.
    fn rewrites_the_row(&self, event: &Progress) -> bool {
        self.counter == Counter::InPlace
            && matches!(
                event,
                Progress::Downloading { .. } | Progress::Extracting { .. }
            )
    }

    /// Rewrites the counter row in place, blanking whatever of the previous one
    /// it does not cover.
    fn rewrite_row(&mut self, text: &str) {
        let width = text.chars().count();
        let pad = self.row.saturating_sub(width);
        eprint!("\r{text}{:pad$}", "");
        self.row = width;
    }

    /// Blanks the counter row, if one is on screen, and returns the cursor to
    /// the start of it.
    fn clear_row(&mut self) {
        if self.row > 0 {
            eprint!("\r{:width$}\r", "", width = self.row);
            self.row = 0;
        }
    }
}

/// The name a provisioning event's root is reported under: the component's own,
/// or `base` for the shared base a layered run bootstraps once.
fn root_label(component: Option<&str>) -> &str {
    component.unwrap_or("base")
}

/// Prints the closing summary: how many components built, failed, and were
/// skipped and why; what the run produced and where; and the names of any failed
/// components.
///
/// A cancelled run says so in the same line, because the counts read very
/// differently when the run stopped short: the components it never reached are
/// among the skipped.
fn print_summary(report: &RunReport) {
    let verdict = if report.cancelled {
        "summary (cancelled)"
    } else {
        "summary"
    };
    eprintln!(
        "src2deb: {verdict}: {} built, {} failed, {} of {} component(s)",
        report.built.len(),
        report.failed.len(),
        skipped_tally(report),
        report.order.len()
    );
    if report.is_success() {
        // What this run produced, which is not what the directory holds: a run
        // that skipped everything produced nothing while the tree still holds
        // every package the runs before it built. Saying "0 artifacts in <dir>"
        // of a directory full of packages reads as a failed run.
        match report.artifact_count() {
            0 => eprintln!(
                "src2deb: no artifacts produced; {} is unchanged",
                report.out_dir.display()
            ),
            count => eprintln!(
                "src2deb: {count} artifact(s) produced, in {}",
                report.out_dir.display()
            ),
        }
    } else {
        let names: Vec<&str> = report
            .failed
            .iter()
            .map(|failed| failed.component.as_str())
            .collect();
        eprintln!("src2deb: failed: {}", names.join(", "));
    }
}

/// The skipped components of a run, counted by why they were skipped: `"26 not
/// selected"`, `"2 already built, 1 cancelled"`, or `"0 skipped"` when none
/// were.
///
/// The three reasons are three different outcomes, and collapsing them makes a
/// deliberate single-component build (`1 built, 26 skipped of 27`) read exactly
/// like a run that fell over. Naming the reason is what makes the closing line
/// say what happened.
fn skipped_tally(report: &RunReport) -> String {
    let counts: Vec<String> = SkipReason::ALL
        .into_iter()
        .filter_map(|reason| match report.skipped_for(reason) {
            0 => None,
            count => Some(format!("{count} {}", reason.label())),
        })
        .collect();
    if counts.is_empty() {
        return "0 skipped".to_string();
    }
    counts.join(", ")
}

/// Prints the resolved build order to stdout, one component per line with the
/// source it resolved to and, when requested, its build-dependencies. Progress
/// goes to stderr, so the plan itself stays cleanly pipeable on stdout.
///
/// A component that declares its version gets a line for it. Most do not, and a
/// line saying so for every component would bury the ones where the version was
/// a decision — for `version-from`, one the plan made.
fn print_plan(report: &PlanReport, show_build_deps: bool) {
    for (position, component) in report.components.iter().enumerate() {
        println!(
            "{:>3}. {} @ {}",
            position + 1,
            component.name,
            plan_source(&component.source)
        );
        if let Some(version) = &component.version {
            println!("     version: {version}");
        }
        if show_build_deps {
            if component.build_deps.is_empty() {
                println!("     build-deps: (none)");
            } else {
                println!("     build-deps: {}", component.build_deps.join(", "));
            }
        }
    }
}

/// Parses a `--build-date` value: a `YYYY-MM-DD` date, or `manifest` to take
/// the date the prior run recorded.
///
/// Checked here rather than inside the run, for the same reason as the suite
/// and the architecture: a date the calendar does not have is a usage error
/// against the flag, and catching it now keeps it from surfacing after the run
/// has cloned every source.
fn parse_build_date(value: &str) -> Result<src2deb::BuildDate, String> {
    if value == "manifest" {
        return Ok(src2deb::BuildDate::Recorded);
    }
    match src2deb::version::epoch_at_date(value) {
        Some(seconds) => Ok(src2deb::BuildDate::At(seconds)),
        None => Err(format!(
            "--build-date value {value:?} is neither a YYYY-MM-DD date nor \"manifest\""
        )),
    }
}

/// A component's source, as the plan lists it: every input the component
/// resolved, separated by commas.
///
/// A single-input component prints its value alone, which is what a reader of a
/// build order expects to see. A component assembled from more than one prints
/// the part each played, since two inputs of the same kind are told apart by
/// nothing else.
///
/// A git revision is printed bare; an input of any other kind is qualified by
/// its kind, so a digest is not mistaken for a commit — except a patch series,
/// whose kind only repeats the part it played. The full values are recorded in
/// the manifest.
fn plan_source(source: &Fingerprint) -> String {
    let labelled = source.len() > 1;
    source
        .inputs()
        .iter()
        .map(|input| {
            let value = match input.kind() {
                SourceKind::Git | SourceKind::Patches => abbreviate(input.value()).to_string(),
                SourceKind::Sha256 | SourceKind::Tree => {
                    format!("{}:{}", input.kind().label(), abbreviate(input.value()))
                }
                // A path abbreviated is a path with its identifying part cut
                // off, so it is printed whole. It stays on this terminal; only
                // the marker in the version reaches a package.
                SourceKind::Path => format!("path:{}", input.value()),
            };
            match labelled {
                true => format!("{} {value}", input.role().label()),
                false => value,
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A hash abbreviated to its first 12 characters for display; the full value is
/// recorded in the manifest.
fn abbreviate(hash: &str) -> &str {
    let end = hash.char_indices().nth(12).map_or(hash.len(), |(i, _)| i);
    &hash[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses from string slices, as the process would from `env::args`.
    fn parse(args: &[&str]) -> Result<Cli, String> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_args(&owned)
    }

    /// Unwraps a parsed `build` command's arguments.
    fn build_args(args: &[&str]) -> BuildArgs {
        match parse(args).unwrap().command {
            Command::Build(build) => build,
            other => panic!("expected a build command, got {other:?}"),
        }
    }

    /// Unwraps a parsed `plan` command's arguments.
    fn plan_args(args: &[&str]) -> PlanArgs {
        match parse(args).unwrap().command {
            Command::Plan(plan) => plan,
            other => panic!("expected a plan command, got {other:?}"),
        }
    }

    #[test]
    fn build_takes_a_recipe_dir_and_defaults_the_work_dir() {
        let args = build_args(&["build", "recipes/cosmic"]);
        assert_eq!(args.recipe_dir, PathBuf::from("recipes/cosmic"));
        assert_eq!(args.work, PathBuf::from("work"));
        assert!(!args.keep_going);
    }

    #[test]
    fn the_work_dir_and_keep_going_are_parsed() {
        let args = build_args(&["build", "r", "--work", "/tmp/w", "--keep-going"]);
        assert_eq!(args.work, PathBuf::from("/tmp/w"));
        assert!(args.keep_going);
    }

    #[test]
    fn the_architecture_override_is_absent_unless_given() {
        assert_eq!(build_args(&["build", "r"]).architecture, None);
        assert_eq!(plan_args(&["plan", "r"]).architecture, None);
    }

    #[test]
    fn the_architecture_override_is_parsed_by_both_subcommands() {
        assert_eq!(
            build_args(&["build", "r", "--architecture", "arm64"]).architecture,
            Some("arm64".to_string()),
        );
        assert_eq!(
            plan_args(&["plan", "r", "--architecture", "riscv64"]).architecture,
            Some("riscv64".to_string()),
        );
    }

    #[test]
    fn the_suite_override_is_absent_unless_given() {
        assert_eq!(build_args(&["build", "r"]).suite, None);
        assert_eq!(plan_args(&["plan", "r"]).suite, None);
    }

    #[test]
    fn the_suite_override_is_parsed_by_both_subcommands() {
        assert_eq!(
            build_args(&["build", "r", "--suite", "forky"]).suite,
            Some("forky".to_string()),
        );
        assert_eq!(
            plan_args(&["plan", "r", "--suite", "trixie"]).suite,
            Some("trixie".to_string()),
        );
    }

    #[test]
    fn the_suite_and_architecture_overrides_are_independent() {
        // One recipe serves every (suite, architecture) it builds against, so
        // both axes are overridable at once and neither implies the other.
        let args = build_args(&["build", "r", "--suite", "forky", "--architecture", "arm64"]);
        assert_eq!(args.suite, Some("forky".to_string()));
        assert_eq!(args.architecture, Some("arm64".to_string()));
    }

    #[test]
    fn an_unsafe_or_missing_suite_value_is_a_usage_error() {
        // The suite becomes a path segment of the pool, output tree, and
        // manifest, and a field in a `sources.list` line.
        assert!(
            parse(&["build", "r", "--suite", "a/b"])
                .unwrap_err()
                .contains("contains a path separator")
        );
        assert!(
            parse(&["build", "r", "--suite", "for ky"])
                .unwrap_err()
                .contains("contains whitespace")
        );
        assert!(
            parse(&["plan", "r", "--suite"])
                .unwrap_err()
                .contains("--suite requires a value")
        );
    }

    #[test]
    fn the_version_tag_override_is_parsed_by_both_subcommands() {
        assert_eq!(build_args(&["build", "r"]).version_tag, None);
        assert_eq!(
            build_args(&["build", "r", "--version-tag", "debsid"]).version_tag,
            Some("debsid".to_string()),
        );
        assert_eq!(
            plan_args(&["plan", "r", "--version-tag", "deb13"]).version_tag,
            Some("deb13".to_string()),
        );
    }

    #[test]
    fn an_unusable_or_missing_version_tag_is_a_usage_error() {
        // The tag is spliced into the Debian revision of every version the run
        // produces, and `-` there would move the boundary between upstream
        // version and revision. See `recipe::version_tag_error`.
        assert!(
            parse(&["build", "r", "--version-tag", "deb-13"])
                .unwrap_err()
                .contains("a character a Debian version may not")
        );
        assert!(
            parse(&["build", "r", "--version-tag"])
                .unwrap_err()
                .contains("--version-tag requires a value")
        );
    }

    #[test]
    fn an_unsafe_or_missing_architecture_value_is_a_usage_error() {
        assert!(
            parse(&["build", "r", "--architecture", "a/b"])
                .unwrap_err()
                .contains("contains a path separator")
        );
        assert!(
            parse(&["plan", "r", "--architecture"])
                .unwrap_err()
                .contains("--architecture requires a value")
        );
    }

    #[test]
    fn jobs_parses_a_positive_integer_and_rejects_the_rest() {
        assert_eq!(build_args(&["build", "r", "--jobs", "4"]).jobs, 4);
        assert!(
            parse(&["build", "r", "--jobs", "0"])
                .unwrap_err()
                .contains("at least 1")
        );
        assert!(
            parse(&["build", "r", "--jobs", "x"])
                .unwrap_err()
                .contains("not a positive integer")
        );
        assert!(
            parse(&["build", "r", "--jobs"])
                .unwrap_err()
                .contains("--jobs requires a value")
        );
    }

    #[test]
    fn selection_defaults_to_all_and_parses_only_and_from() {
        assert_eq!(build_args(&["build", "r"]).selection, Selection::All);
        assert_eq!(
            build_args(&["build", "r", "--only", "a", "--only", "b"]).selection,
            Selection::Only(vec!["a".to_string(), "b".to_string()]),
        );
        assert_eq!(
            build_args(&["build", "r", "--from", "c"]).selection,
            Selection::From("c".to_string()),
        );
    }

    #[test]
    fn only_and_from_are_mutually_exclusive() {
        assert!(
            parse(&["build", "r", "--only", "a", "--from", "b"])
                .unwrap_err()
                .contains("mutually exclusive")
        );
    }

    #[test]
    fn skip_published_is_a_flag() {
        assert!(!build_args(&["build", "r"]).skip_published);
        assert!(build_args(&["build", "r", "--skip-published"]).skip_published);
    }

    #[test]
    fn options_may_precede_the_positional_recipe_dir() {
        // The parser collects options and positionals independently, so order
        // between them does not matter.
        let args = build_args(&["build", "--keep-going", "--work", "w", "r"]);
        assert_eq!(args.recipe_dir, PathBuf::from("r"));
        assert!(args.keep_going);
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(parse(&["--help"]).unwrap().command, Command::Help);
        assert_eq!(parse(&["-h"]).unwrap().command, Command::Help);
        assert_eq!(parse(&["--version"]).unwrap().command, Command::Version);
        assert_eq!(parse(&["-V"]).unwrap().command, Command::Version);
        // A help request wins even alongside an otherwise-valid build.
        assert_eq!(
            parse(&["build", "r", "--help"]).unwrap().command,
            Command::Help
        );
    }

    #[test]
    fn a_missing_subcommand_is_a_usage_error() {
        assert!(parse(&[]).unwrap_err().contains("subcommand is required"));
    }

    #[test]
    fn an_unknown_subcommand_is_rejected() {
        assert!(
            parse(&["frobnicate", "r"])
                .unwrap_err()
                .contains("unknown subcommand")
        );
    }

    #[test]
    fn build_requires_exactly_one_recipe_dir() {
        assert!(
            parse(&["build"])
                .unwrap_err()
                .contains("a RECIPE_DIR is required")
        );
        assert!(
            parse(&["build", "a", "b"])
                .unwrap_err()
                .contains("takes exactly one RECIPE_DIR")
        );
    }

    #[test]
    fn work_requires_a_value() {
        assert!(
            parse(&["build", "r", "--work"])
                .unwrap_err()
                .contains("--work requires a value")
        );
    }

    #[test]
    fn an_unrecognized_option_is_rejected() {
        assert!(
            parse(&["build", "r", "--nope"])
                .unwrap_err()
                .contains("unrecognized option --nope")
        );
    }

    #[test]
    fn plan_parses_its_recipe_dir_work_and_build_deps_flag() {
        let args = plan_args(&["plan", "r"]);
        assert_eq!(args.recipe_dir, PathBuf::from("r"));
        assert_eq!(args.work, PathBuf::from("work"));
        assert!(!args.show_build_deps);

        let args = plan_args(&["plan", "r", "--work", "w", "--build-deps"]);
        assert_eq!(args.work, PathBuf::from("w"));
        assert!(args.show_build_deps);
    }

    #[test]
    fn each_subcommand_rejects_the_other_subcommands_flags() {
        // `--build-deps` belongs to `plan`, `--keep-going` to `build`; neither
        // leaks into the other.
        assert!(
            parse(&["build", "r", "--build-deps"])
                .unwrap_err()
                .contains("unrecognized option --build-deps")
        );
        assert!(
            parse(&["plan", "r", "--keep-going"])
                .unwrap_err()
                .contains("unrecognized option --keep-going")
        );
    }

    #[test]
    fn verbosity_defaults_to_normal_and_the_last_flag_wins() {
        assert_eq!(build_args(&["build", "r"]).verbosity, Verbosity::Normal);
        assert_eq!(
            build_args(&["build", "r", "-q"]).verbosity,
            Verbosity::Quiet
        );
        assert_eq!(
            plan_args(&["plan", "r", "--verbose"]).verbosity,
            Verbosity::Verbose
        );
        // Both given: the last on the line wins.
        assert_eq!(
            build_args(&["build", "r", "-v", "-q"]).verbosity,
            Verbosity::Quiet
        );
        assert_eq!(
            build_args(&["build", "r", "-q", "-v"]).verbosity,
            Verbosity::Verbose
        );
    }

    /// A git source at `commit`, the shape the resolver produces.
    fn git(commit: &str) -> Fingerprint {
        Fingerprint::of(src2deb::SourceInput::git(
            src2deb::SourceRole::Source,
            commit,
        ))
    }

    /// A report with the given outcome counts, for exercising the exit status.
    fn report(built: usize, failed: usize, cancelled: bool) -> RunReport {
        RunReport {
            order: Vec::new(),
            built: (0..built)
                .map(|n| src2deb::Built {
                    component: format!("built-{n}"),
                    source: git("abc"),
                    version: None,
                    artifacts: vec![src2deb::Artifact {
                        package: format!("built-{n}"),
                        version: "1.0".to_string(),
                        path: PathBuf::from("/out/built.deb"),
                    }],
                    buildinfo: None,
                    packages: Vec::new(),
                })
                .collect(),
            failed: (0..failed)
                .map(|n| src2deb::Failed {
                    component: format!("failed-{n}"),
                    source: git("abc"),
                    error: src2deb::Error::Pool("boom".to_string()),
                })
                .collect(),
            skipped: Vec::new(),
            cancelled,
            out_dir: PathBuf::from("/out"),
            manifest_path: PathBuf::from("/m"),
        }
    }

    /// Writes a recipe directory holding `toml` and returns its path.
    fn recipe_dir(label: &str, toml: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("src2deb-cli-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("recipe.toml"), toml).unwrap();
        dir
    }

    /// A recipe declaring a suite src2deb does not know, and its own tag for it.
    const TAGGED: &str = "\
name = \"tagged\"
suite = \"sid\"
version-tag = \"debsid\"
architecture = \"amd64\"

[[components]]
name = \"c\"
source.git = \"https://example.invalid/c\"
";

    #[test]
    fn a_recipes_version_tag_belongs_to_the_suite_it_declares() {
        let dir = recipe_dir("tag", TAGGED);

        // Untouched, the recipe stamps the tag it declares for the suite it
        // declares.
        let recipe = load_recipe(&dir, None, None, None, None).unwrap();
        assert_eq!(recipe.resolved_version_tag(), Some("debsid"));

        // Retargeted, that tag described a suite the run is no longer building
        // for, so the new suite's own tag stands. Carrying `debsid` onto a
        // trixie build would stamp packages with the name of a release they
        // were not built against.
        let recipe = load_recipe(&dir, Some("trixie".to_string()), None, None, None).unwrap();
        assert_eq!(recipe.suite, "trixie");
        assert_eq!(recipe.resolved_version_tag(), Some("deb13"));

        // A suite with no derivable tag is refused rather than guessed, and the
        // refusal names the flag that settles it.
        let err =
            load_recipe(&dir, Some("experimental".to_string()), None, None, None).unwrap_err();
        let Fault::Usage(message) = err else {
            panic!("expected a usage error");
        };
        assert!(message.contains("--version-tag"), "{message}");

        // Which the flag then does, with no edit to the recipe.
        let recipe = load_recipe(
            &dir,
            Some("experimental".to_string()),
            None,
            None,
            Some("debexp".to_string()),
        )
        .unwrap();
        assert_eq!(recipe.resolved_version_tag(), Some("debexp"));

        // The flag also overrides a tag the recipe would otherwise resolve on
        // its own, since it is the more specific statement of the two.
        let recipe = load_recipe(&dir, None, None, None, Some("debexp".to_string())).unwrap();
        assert_eq!(recipe.suite, "sid");
        assert_eq!(recipe.resolved_version_tag(), Some("debexp"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A report whose skipped components carry `reasons`.
    fn skipped_report(reasons: &[SkipReason]) -> RunReport {
        RunReport {
            skipped: reasons
                .iter()
                .enumerate()
                .map(|(n, reason)| src2deb::Skipped {
                    component: format!("skipped-{n}"),
                    source: git("abc"),
                    reason: *reason,
                })
                .collect(),
            ..report(0, 0, false)
        }
    }

    #[test]
    fn the_summary_says_why_components_were_skipped() {
        use SkipReason::{AlreadyBuilt, Cancelled, NotSelected};

        assert_eq!(skipped_tally(&skipped_report(&[])), "0 skipped");
        // A deliberate single-component build over a large recipe reads as a
        // deliberate one rather than as a run that fell over.
        assert_eq!(
            skipped_tally(&skipped_report(&[NotSelected, NotSelected])),
            "2 not selected"
        );
        // Several reasons at once are broken out, in a fixed order so the line
        // reads the same run to run.
        assert_eq!(
            skipped_tally(&skipped_report(&[Cancelled, NotSelected, AlreadyBuilt])),
            "1 already built, 1 not selected, 1 cancelled"
        );
    }

    #[test]
    fn the_exit_status_names_why_the_run_ended() {
        assert_eq!(exit_status(&report(2, 0, false)), 0);
        assert_eq!(exit_status(&report(1, 1, false)), 1);
        assert_eq!(exit_status(&report(2, 0, true)), CANCELLED_EXIT);
        // A run that was cancelled after a component failed reports the
        // cancellation: it never finished, so its failure count is not the
        // whole story.
        assert_eq!(exit_status(&report(1, 1, true)), CANCELLED_EXIT);
    }

    #[test]
    fn the_counter_style_follows_the_verbosity_the_job_count_and_the_stream() {
        // Verbosity decides first: quiet prints no counter and verbose prints
        // every package, whatever the run's shape.
        for jobs in [1, 4] {
            for terminal in [false, true] {
                assert_eq!(
                    Counter::of(Verbosity::Quiet, jobs, terminal),
                    Counter::Silent
                );
                assert_eq!(
                    Counter::of(Verbosity::Verbose, jobs, terminal),
                    Counter::PerPackage
                );
            }
        }
        // A rewritten row needs a sequential run on a terminal: concurrent
        // workers would fight over the row, and a redirected stream would
        // collect carriage returns instead of lines.
        assert_eq!(Counter::of(Verbosity::Normal, 1, true), Counter::InPlace);
        assert_eq!(Counter::of(Verbosity::Normal, 4, true), Counter::Periodic);
        assert_eq!(Counter::of(Verbosity::Normal, 1, false), Counter::Periodic);
        assert_eq!(Counter::of(Verbosity::Normal, 4, false), Counter::Periodic);
    }

    #[test]
    fn the_shared_base_reports_under_its_own_label() {
        assert_eq!(root_label(None), "base");
        assert_eq!(root_label(Some("cosmic-comp")), "cosmic-comp");
    }

    #[test]
    fn build_date_takes_a_calendar_date_or_the_prior_manifests() {
        assert_eq!(
            build_args(&["build", "r", "--build-date", "2026-07-31"]).build_date,
            src2deb::BuildDate::At(1_785_456_000),
        );
        assert_eq!(
            build_args(&["build", "r", "--build-date", "manifest"]).build_date,
            src2deb::BuildDate::Recorded,
        );
        // Unset, a run is dated as it always has been.
        assert_eq!(
            build_args(&["build", "r"]).build_date,
            src2deb::BuildDate::Now,
        );
    }

    #[test]
    fn an_unusable_build_date_is_a_usage_error_against_the_flag() {
        // Caught at parse time, so it never surfaces after the run has cloned
        // every source.
        for value in ["2026-02-30", "yesterday", "2026-7-31", ""] {
            let err = parse(&["build", "r", "--build-date", value])
                .unwrap_err()
                .to_string();
            assert!(err.contains("--build-date"), "{value:?}: {err}");
        }
        assert!(
            parse(&["build", "r", "--build-date"])
                .unwrap_err()
                .contains("requires a value")
        );
    }

    #[test]
    fn a_hash_is_abbreviated_to_its_first_twelve_characters() {
        assert_eq!(abbreviate("0123456789abcdef0123"), "0123456789ab");
        // A shorter string (an empty or partial hash) is returned whole.
        assert_eq!(abbreviate("abc"), "abc");
        assert_eq!(abbreviate(""), "");
    }

    #[test]
    fn the_plan_prints_a_git_revision_bare_and_qualifies_every_other_kind() {
        use src2deb::{SourceInput, SourceRole};

        // A build order reads as a list of revisions, so a commit is printed as
        // one. Anything else is named, because an unqualified digest or path
        // would be read as a commit that is not one.
        assert_eq!(plan_source(&git("0123456789abcdef0123")), "0123456789ab");
        assert_eq!(
            plan_source(&Fingerprint::of(SourceInput::sha256(
                SourceRole::Source,
                "9f8e7d6c5b4a39281706",
            ))),
            "sha256:9f8e7d6c5b4a",
        );
        // A path abbreviated is a path with its identifying part cut off, so it
        // is printed whole.
        assert_eq!(
            plan_source(&Fingerprint::of(SourceInput::path(
                SourceRole::Source,
                "/home/someone/cosmic-comp",
            ))),
            "path:/home/someone/cosmic-comp",
        );
    }

    #[test]
    fn the_plan_names_the_part_each_input_played_once_there_is_more_than_one() {
        use src2deb::{SourceInput, SourceRole};

        // Two inputs of one kind are told apart by nothing else, and a build
        // order that listed two bare commits would not say which was which.
        assert_eq!(
            plan_source(&Fingerprint::over(vec![
                SourceInput::git(SourceRole::Source, "0123456789abcdef0123"),
                SourceInput::git(SourceRole::Packaging, "fedcba9876543210fedc"),
                SourceInput::patches("9f8e7d6c5b4a39281706"),
            ])),
            "source 0123456789ab, packaging fedcba987654, patches 9f8e7d6c5b4a",
        );
    }
}
