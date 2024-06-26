use clap::{Parser, Subcommand};
use rayon::iter::{ParallelBridge, ParallelIterator};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};
use walkdir::WalkDir;

#[derive(clap::Parser, Debug, PartialEq)]
struct Opt {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug, PartialEq)]
enum Command {
    // path to rust-project.json
    Json { path: PathBuf },
    // path to manifest
    Cargo { path: PathBuf },
}

fn main() -> Result<(), anyhow::Error> {
    let opt = Opt::parse();
    match opt.command {
        Command::Json { path } => handle_project_json(&path),
        Command::Cargo { path } => handle_cargo(&path),
    }?;

    Ok(())
}

fn handle_cargo(path: &Path) -> Result<(), anyhow::Error> {
    let instant = Instant::now();
    let mut cmd = cargo_metadata::MetadataCommand::new();
    cmd.manifest_path(path);
    let metadata = cmd.exec()?;
    eprintln!(
        "Done running cargo-metadata: {}ms",
        instant.elapsed().as_millis()
    );

    let instant = Instant::now();
    let _projects: FxHashMap<String, Result<Vec<String>, std::io::Error>> = metadata
        .packages
        .into_iter()
        .flat_map(|package| package.targets)
        .filter_map(|target| {
            let root = target.src_path.parent();
            match root {
                Some(path) => Some((target.name.clone(), path.to_path_buf())),
                None => None,
            }
        })
        .par_bridge()
        .map(|(name, dir)| {
            let dir_contents = WalkDir::new(dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .map(|f| std::fs::read_to_string(f.path()))
                .collect::<Result<Vec<String>, std::io::Error>>();
            (name, dir_contents)
        })
        .collect();

    eprintln!("Done loading: {}ms", instant.elapsed().as_millis());

    Ok(())
}

fn handle_project_json(path: &Path) -> Result<(), anyhow::Error> {
    let s = std::fs::read_to_string(path)?;
    let project: JsonProject = serde_json::from_str(&s)?;

    let instant = Instant::now();
    let projects: FxHashMap<String, Result<Vec<String>, std::io::Error>> = project
        .crates
        .iter()
        .filter_map(|krate| {
            let root = krate.root_module.parent();
            match root {
                Some(path) => Some((krate.display_name.clone().unwrap(), path.to_path_buf())),
                None => None,
            }
        })
        .par_bridge()
        .map(|(name, dir)| {
            let dir_contents = WalkDir::new(dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .map(|f| std::fs::read_to_string(f.path()))
                .collect::<Result<Vec<String>, std::io::Error>>();
            (name, dir_contents)
        })
        .collect();

    eprintln!("Done loading: {}", instant.elapsed().as_millis());
    eprintln!("{:?}", projects.keys());

    Ok(())
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct JsonProject {
    #[serde(flatten)]
    pub(crate) sysroot: Sysroot,

    /// The set of crates comprising the project.
    ///
    /// Must include all transitive dependencies as well as sysroot crate (libstd,
    /// libcore, etc.).
    pub(crate) crates: Vec<Crate>,
    pub(crate) runnables: Vec<Runnable>,
    pub(crate) generated: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Crate {
    /// Optional crate name used for display purposes; has no semantic significance.
    pub(crate) display_name: Option<String>,
    /// The path to the root module of the crate.
    pub(crate) root_module: PathBuf,
    pub(crate) edition: Edition,
    pub(crate) deps: Vec<Dep>,
    /// Should this crate be treated as a member of
    /// current "workspace".
    ///
    /// By default, inferred from the `root_module`
    /// (members are the crates which reside inside
    /// the directory opened in the editor).
    ///
    /// Set this to `false` for things like standard
    /// library and 3rd party crates to enable
    /// performance optimizations (rust-analyzer
    /// assumes that non-member crates don't change).
    pub(crate) is_workspace_member: bool,
    /// Optionally specify the (super)set of `.rs`
    /// files comprising this crate.
    ///
    /// By default, rust-analyzer assumes that only
    /// files under `root_module.parent` can belong
    /// to a crate. `include_dirs` are included
    /// recursively, unless a subdirectory is in
    /// `exclude_dirs`.
    ///
    /// Different crates can share the same `source`.
    ///
    /// If two crates share an `.rs` file in common,
    /// they *must* have the same `source`.
    /// rust-analyzer assumes that files from one
    /// source can't refer to files in another source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<Source>,
    /// The set of cfgs activated for a given crate.
    ///
    /// With how fb imports crates into fbsource/third-party,
    /// the answer is "all of them".
    pub(crate) cfg: Vec<String>,
    /// The target triple for a given crate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) build: Option<Build>,
    /// Environment for the crate, often used by `env!`.
    pub(crate) env: FxHashMap<String, String>,
    /// Whether the crate is a proc-macro crate/
    pub(crate) is_proc_macro: bool,
    /// For proc-macro crates, path to compiled
    /// proc-macro (.so, .dylib, or .dll. depends on the platform.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) proc_macro_dylib_path: Option<PathBuf>,
}

/// Build system-specific additions the `rust-project.json`.
///
/// rust-analyzer encodes Cargo-specific knowledge in features
/// such as flycheck or runnable and constructs Cargo-specific commands
/// on the fly. This is a reasonable decision on its part, as most people
/// use Cargo. However, to support equivalent functionality with non-Cargo
/// build systems in rust-analyzer, this struct encodes pre-defined runnables
/// and other bits of metadata. Below is an example of `TargetSpec` in JSON:
///
/// ```json
/// "target_spec": {
///     "manifest_file": "/Users/dbarsky/fbsource/fbcode/buck2/integrations/rust-project/TARGETS",
///     "target_label": "fbcode//buck2/integrations/rust-project:rust-project",
///     "target_kind": "bin",
///     "runnables": {
///         "check": [
///            "build",
///            "fbcode//buck2/integrations/rust-project:rust-project"
///         ],
///         "run": [
///             "run",
///             "fbcode//buck2/integrations/rust-project:rust-project"
///         ],
///         "test": [
///             "test",
///             "fbcode//buck2/integrations/rust-project:rust-project",
///             "--",
///             "{test_id}",
///             "--print-passing-details"
///         ]
///     },
///     "flycheck_command": [
///         "build",
///         "fbcode//buck2/integrations/rust-project:rust-project"
///     ]
/// }
/// ```
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Build {
    pub(crate) label: String,
    /// `build_file` corresponds to the `BUCK`/`TARGETS` file.
    pub(crate) build_file: PathBuf,
    pub(crate) target_kind: TargetKind,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TargetKind {
    #[default]
    Bin,
    /// Any kind of Cargo lib crate-type (dylib, rlib, proc-macro, ...).
    Lib,
    Example,
    Test,
    Bench,
    BuildScript,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Runnable {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub kind: RunnableKind,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RunnableKind {
    Check,
    Flycheck,
    Run,
    TestOne,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(rename = "edition")]
pub(crate) enum Edition {
    #[serde(rename = "2015")]
    Edition2015,
    #[serde(rename = "2018")]
    Edition2018,
    #[default]
    #[serde(rename = "2021")]
    Edition2021,
}

/// An optional set of Rust files that comprise the crate.
///
/// By default, rust-analyzer assumes that only files under
/// `Crate::root_module` can belong to a crate. `include_dirs`
/// are included recursively, unless a subdirectory is
/// specified in `include_dirs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct Source {
    pub(crate) include_dirs: FxHashSet<PathBuf>,
    pub(crate) exclude_dirs: FxHashSet<PathBuf>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Dep {
    #[serde(rename = "crate")]
    pub(crate) crate_index: usize,
    pub(crate) name: String,
}

/// Sysroot paths. These are documented in the rust-analyzer manual:
///
/// <https://rust-analyzer.github.io/manual.html#non-cargo-based-projects>
///
/// rust-analyzer treats both paths as optional, but we always provide sysroot.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sysroot {
    /// Path to the directory of the sysroot; this is a superset of `sysroot_src`.
    ///
    /// This path provides rust-analyzer both the *source code* of libraries
    /// like `std` and `core` and binaries like `rust-analyzer-proc-macro-srv`,
    /// which enable rust-analyzer to expand procedural macros.
    ///
    /// For example, a `sysroot` is `~/fbsource/fbcode/third-party-buck/platform010/build/rust/`.
    ///
    /// `rust-analyzer` relies on an external binary to expand procedural
    /// macros and the source code location can be predictably inferred.
    /// Assuming the example sysroot above, the source code would be located in
    /// `/lib/rustlib/src/rust/`.
    pub(crate) sysroot: PathBuf,
    /// Legacy sysroot config containing only the source code of libraries such
    /// as `std` and core`.
    ///
    /// Inside Meta, this is necessary on non-Linux platforms since the sources
    /// are packaged seperately from binaries such as `rust-analyzer-proc-macro-srv`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sysroot_src: Option<PathBuf>,
}
