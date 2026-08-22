use anyhow::{Context, Result, bail};
use cargo_metadata::{Metadata, Package, PackageId, Target};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub(crate) fn load(
    current_dir: &Path,
    manifest_path: Option<&Path>,
    locked: bool,
    offline: bool,
    frozen: bool,
) -> Result<Metadata> {
    let mut command = cargo_metadata::MetadataCommand::new();
    command.current_dir(current_dir).no_deps();
    if let Some(path) = manifest_path {
        command.manifest_path(path);
    }
    if let Some(cargo) = std::env::var_os("CARGO") {
        command.cargo_path(PathBuf::from(cargo));
    }
    let mut options = Vec::<String>::new();
    append_flag(&mut options, "--locked", locked);
    append_flag(&mut options, "--offline", offline);
    append_flag(&mut options, "--frozen", frozen);
    command.other_options(options);
    command.exec().context("could not read Cargo metadata")
}

fn append_flag(options: &mut Vec<String>, flag: &str, enabled: bool) {
    if enabled {
        options.push(flag.to_owned());
    }
}

#[derive(Debug)]
pub(crate) struct SelectionContext {
    pub(crate) package: Option<String>,
    pub(crate) binary: Option<String>,
    pub(crate) binaries: Vec<String>,
    pub(crate) all_bins: bool,
    pub(crate) all: bool,
    pub(crate) manifest_path: Option<PathBuf>,
    pub(crate) current_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct Selection {
    pub(crate) package_id: PackageId,
    pub(crate) package_name: String,
    pub(crate) binary_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuildScope {
    WorkspaceAllBins,
    PackageAllBins,
    Selected,
}

#[derive(Debug)]
pub(crate) struct SelectionSet {
    pub(crate) binaries: Vec<Selection>,
    pub(crate) build_scope: BuildScope,
}

#[cfg(test)]
pub(crate) fn select(metadata: &Metadata, context: &SelectionContext) -> Result<Selection> {
    let mut selected = select_many(metadata, context)?;
    if selected.binaries.len() != 1 {
        bail!("selection produced more than one Cargo binary");
    }
    Ok(selected.binaries.remove(0))
}

pub(crate) fn select_many(metadata: &Metadata, context: &SelectionContext) -> Result<SelectionSet> {
    validate_selector_combination(context)?;
    if context.all {
        return select_workspace_binaries(metadata);
    }

    let package = select_package(metadata, context)?;
    let binaries = if context.all_bins {
        binary_targets(package)?
    } else if !context.binaries.is_empty() {
        select_named_binaries(package, &context.binaries)?
    } else {
        vec![select_binary(package, context.binary.as_deref())?]
    };
    let build_scope = if context.all_bins {
        BuildScope::PackageAllBins
    } else {
        BuildScope::Selected
    };
    Ok(SelectionSet {
        binaries: binaries
            .into_iter()
            .map(|binary| selection(package, binary))
            .collect(),
        build_scope,
    })
}

fn validate_selector_combination(context: &SelectionContext) -> Result<()> {
    let binary_modes = usize::from(context.binary.is_some())
        + usize::from(!context.binaries.is_empty())
        + usize::from(context.all_bins)
        + usize::from(context.all);
    if binary_modes > 1 {
        bail!("--bin, --bins, --all-bins, and --all are mutually exclusive");
    }
    if context.all && context.package.is_some() {
        bail!("--all cannot be used with --package");
    }
    Ok(())
}

fn select_workspace_binaries(metadata: &Metadata) -> Result<SelectionSet> {
    let mut packages = metadata.workspace_packages();
    packages.sort_by(|left, right| left.name.cmp(&right.name));
    let mut binaries = Vec::new();
    let mut names = BTreeMap::<String, String>::new();
    for package in packages {
        let mut targets: Vec<_> = package
            .targets
            .iter()
            .filter(|target| target.is_bin())
            .collect();
        targets.sort_by(|left, right| left.name.cmp(&right.name));
        for target in targets {
            if let Some(existing) = names.insert(target.name.clone(), package.name.to_string()) {
                bail!(
                    "binary name `{}` is shared by workspace packages `{existing}` and `{}`; select a subset",
                    target.name,
                    package.name
                );
            }
            binaries.push(selection(package, target));
        }
    }
    if binaries.is_empty() {
        bail!("workspace has no binary targets");
    }
    Ok(SelectionSet {
        binaries,
        build_scope: BuildScope::WorkspaceAllBins,
    })
}

fn select_named_binaries<'a>(
    package: &'a Package,
    requested: &[String],
) -> Result<Vec<&'a Target>> {
    let binaries = binary_targets(package)?;
    let mut names = BTreeSet::new();
    let mut selected = Vec::with_capacity(requested.len());
    for name in requested {
        if !names.insert(name.as_str()) {
            bail!("binary `{name}` was selected more than once");
        }
        let target = binaries
            .iter()
            .copied()
            .find(|target| target.name == *name)
            .with_context(|| {
                format!(
                    "binary `{name}` does not exist in package `{}`; available binaries: {}",
                    package.name,
                    binary_names(&binaries)
                )
            })?;
        selected.push(target);
    }
    selected.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(selected)
}

fn selection(package: &Package, binary: &Target) -> Selection {
    Selection {
        package_id: package.id.clone(),
        package_name: package.name.to_string(),
        binary_name: binary.name.clone(),
    }
}

fn select_package<'a>(metadata: &'a Metadata, context: &SelectionContext) -> Result<&'a Package> {
    let workspace_packages = metadata.workspace_packages();
    if let Some(requested) = context.package.as_deref() {
        let matches: Vec<_> = workspace_packages
            .iter()
            .copied()
            .filter(|package| package.name == requested)
            .collect();
        return match matches.as_slice() {
            [package] => Ok(package),
            [] => bail!(
                "package `{requested}` is not a workspace member; available packages: {}",
                package_names(&workspace_packages)
            ),
            _ => bail!(
                "package `{requested}` is ambiguous; select one package with a more specific --package value"
            ),
        };
    }

    // An explicit --manifest-path names one package. Falling through to cwd
    // inference when it does not match would build a different package than the
    // one the caller pointed at, and Cargo would accept the mismatched pair.
    if let Some(manifest_path) = context.manifest_path.as_deref() {
        let manifest_path = absolute_path(manifest_path, &context.current_dir);
        return workspace_packages
            .iter()
            .copied()
            .find(|package| {
                absolute_path(package.manifest_path.as_std_path(), &context.current_dir)
                    == manifest_path
            })
            .with_context(|| {
                format!(
                    "`{}` is not the manifest of a workspace member; available packages: {}",
                    manifest_path.display(),
                    package_names(&workspace_packages)
                )
            });
    }

    if let Some(package) = package_containing(&workspace_packages, &context.current_dir) {
        return Ok(package);
    }
    if let Some(package) = metadata.root_package() {
        return Ok(package);
    }

    if metadata.workspace_default_members.is_available()
        && metadata.workspace_default_members.len() == 1
    {
        let id = &metadata.workspace_default_members[0];
        return workspace_packages
            .iter()
            .copied()
            .find(|package| &package.id == id)
            .with_context(|| format!("Cargo metadata names unknown default package `{id}`"));
    }

    let choices = if metadata.workspace_default_members.is_available()
        && !metadata.workspace_default_members.is_empty()
    {
        workspace_packages
            .iter()
            .copied()
            .filter(|package| metadata.workspace_default_members.contains(&package.id))
            .collect()
    } else {
        workspace_packages
    };
    bail!(
        "Cargo package is ambiguous; select one with --package <PACKAGE> (available: {})",
        package_names(&choices)
    )
}

fn package_containing<'a>(packages: &[&'a Package], current_dir: &Path) -> Option<&'a Package> {
    packages
        .iter()
        .copied()
        .filter_map(|package| {
            let directory = package.manifest_path.parent()?;
            current_dir
                .starts_with(directory.as_std_path())
                .then_some((directory.as_str().len(), package))
        })
        .max_by_key(|(length, _)| *length)
        .map(|(_, package)| package)
}

fn select_binary<'a>(package: &'a Package, requested: Option<&str>) -> Result<&'a Target> {
    let binaries = binary_targets(package)?;

    if let Some(requested) = requested {
        return binaries
            .iter()
            .copied()
            .find(|target| target.name == requested)
            .with_context(|| {
                format!(
                    "binary `{requested}` does not exist in package `{}`; available binaries: {}",
                    package.name,
                    binary_names(&binaries)
                )
            });
    }

    if let Some(default_run) = package.default_run.as_deref() {
        return binaries
            .iter()
            .copied()
            .find(|target| target.name == default_run)
            .with_context(|| {
                format!(
                    "package `{}` declares unknown default-run binary `{default_run}`",
                    package.name
                )
            });
    }
    if let Some(binary) = binaries
        .iter()
        .copied()
        .find(|target| target.name == package.name.as_ref())
    {
        return Ok(binary);
    }
    if let [binary] = binaries.as_slice() {
        return Ok(binary);
    }

    bail!(
        "package `{}` has multiple binaries; select one with --bin <NAME> (available: {})",
        package.name,
        binary_names(&binaries)
    )
}

fn binary_targets(package: &Package) -> Result<Vec<&Target>> {
    let mut binaries: Vec<_> = package
        .targets
        .iter()
        .filter(|target| target.is_bin())
        .collect();
    binaries.sort_by(|left, right| left.name.cmp(&right.name));
    if binaries.is_empty() {
        bail!("package `{}` has no binary targets", package.name);
    }
    Ok(binaries)
}

/// Absolute *and* lexically normalized. `Path` equality compares components and
/// keeps `..`, so `ws/worker/../tools/Cargo.toml` would otherwise never equal
/// the `ws/tools/Cargo.toml` Cargo reports.
fn absolute_path(path: &Path, current_dir: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    };
    elfpak::paths::normalize_absolute(&absolute)
}

fn package_names(packages: &[&Package]) -> String {
    let mut names: Vec<_> = packages
        .iter()
        .map(|package| package.name.as_ref())
        .collect();
    names.sort_unstable();
    names.join(", ")
}

fn binary_names(binaries: &[&Target]) -> String {
    binaries
        .iter()
        .map(|target| target.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::{SelectionContext, select, select_many};
    use cargo_metadata::Metadata;
    use std::path::PathBuf;

    const WORKSPACE: &str = r#"
    {
      "packages": [
        {
          "name": "api",
          "version": "0.1.0",
          "id": "path+file:///workspace/api#0.1.0",
          "dependencies": [],
          "targets": [
            {"name":"serve","kind":["bin"],"src_path":"/workspace/api/src/bin/serve.rs"},
            {"name":"migrate","kind":["bin"],"src_path":"/workspace/api/src/bin/migrate.rs"}
          ],
          "features": {},
          "manifest_path": "/workspace/api/Cargo.toml",
          "default_run": "serve"
        },
        {
          "name": "worker",
          "version": "0.1.0",
          "id": "path+file:///workspace/worker#0.1.0",
          "dependencies": [],
          "targets": [
            {"name":"worker","kind":["bin"],"src_path":"/workspace/worker/src/main.rs"}
          ],
          "features": {},
          "manifest_path": "/workspace/worker/Cargo.toml"
        }
      ],
      "workspace_members": [
        "path+file:///workspace/api#0.1.0",
        "path+file:///workspace/worker#0.1.0"
      ],
      "workspace_default_members": [
        "path+file:///workspace/api#0.1.0",
        "path+file:///workspace/worker#0.1.0"
      ],
      "resolve": null,
      "target_directory": "/workspace/target",
      "build_directory": "/workspace/target",
      "workspace_root": "/workspace",
      "metadata": {},
      "version": 1
    }
    "#;

    fn workspace() -> Metadata {
        serde_json::from_str(WORKSPACE).unwrap()
    }

    fn context(package: Option<&str>, binary: Option<&str>) -> SelectionContext {
        SelectionContext {
            package: package.map(str::to_owned),
            binary: binary.map(str::to_owned),
            binaries: Vec::new(),
            all_bins: false,
            all: false,
            manifest_path: None,
            current_dir: PathBuf::from("/workspace"),
        }
    }

    fn selected_names(selected: &super::SelectionSet) -> Vec<(&str, &str)> {
        selected
            .binaries
            .iter()
            .map(|binary| (binary.package_name.as_str(), binary.binary_name.as_str()))
            .collect()
    }

    #[test]
    fn all_selects_every_workspace_binary_in_stable_order() {
        let mut selected_context = context(None, None);
        selected_context.all = true;

        let selected = select_many(&workspace(), &selected_context).unwrap();

        assert_eq!(
            selected_names(&selected),
            [("api", "migrate"), ("api", "serve"), ("worker", "worker")]
        );
        assert_eq!(selected.build_scope, super::BuildScope::WorkspaceAllBins);
    }

    #[test]
    fn all_bins_selects_every_binary_in_one_package() {
        let mut selected_context = context(Some("api"), None);
        selected_context.all_bins = true;

        let selected = select_many(&workspace(), &selected_context).unwrap();

        assert_eq!(
            selected_names(&selected),
            [("api", "migrate"), ("api", "serve")]
        );
        assert_eq!(selected.build_scope, super::BuildScope::PackageAllBins);
    }

    #[test]
    fn bins_selects_a_named_subset_in_one_package() {
        let mut selected_context = context(Some("api"), None);
        selected_context.binaries = vec!["migrate".to_string()];

        let selected = select_many(&workspace(), &selected_context).unwrap();

        assert_eq!(selected_names(&selected), [("api", "migrate")]);
        assert_eq!(selected.build_scope, super::BuildScope::Selected);
    }

    #[test]
    fn all_rejects_binary_names_shared_by_packages() {
        let mut metadata = workspace();
        metadata.packages[1].targets[0].name = "serve".to_string();
        let mut selected_context = context(None, None);
        selected_context.all = true;

        let error = select_many(&metadata, &selected_context).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("serve"), "{text}");
        assert!(text.contains("api"), "{text}");
        assert!(text.contains("worker"), "{text}");
    }

    #[test]
    fn all_skips_workspace_packages_without_binary_targets() {
        let mut metadata = workspace();
        metadata.packages[1].targets.clear();
        let mut selected_context = context(None, None);
        selected_context.all = true;

        let selected = select_many(&metadata, &selected_context).unwrap();

        assert_eq!(
            selected_names(&selected),
            [("api", "migrate"), ("api", "serve")]
        );
    }

    #[test]
    fn bins_reports_an_unknown_name_with_available_choices() {
        let mut selected_context = context(Some("api"), None);
        selected_context.binaries = vec!["missing".to_string()];

        let error = select_many(&workspace(), &selected_context).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("missing"), "{text}");
        assert!(text.contains("migrate, serve"), "{text}");
    }

    #[test]
    fn explicit_package_selects_its_package_named_binary() {
        let selected = select(&workspace(), &context(Some("worker"), None)).unwrap();
        assert_eq!(selected.package_name, "worker");
        assert_eq!(selected.binary_name, "worker");
    }

    #[test]
    fn default_run_selects_a_binary() {
        let selected = select(&workspace(), &context(Some("api"), None)).unwrap();
        assert_eq!(selected.binary_name, "serve");
    }

    #[test]
    fn explicit_binary_overrides_default_run() {
        let selected = select(&workspace(), &context(Some("api"), Some("migrate"))).unwrap();
        assert_eq!(selected.binary_name, "migrate");
    }

    #[test]
    fn ambiguous_workspace_requires_package_and_lists_choices() {
        let error = select(&workspace(), &context(None, None)).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("--package"), "{text}");
        assert!(text.contains("api, worker"), "{text}");
    }

    #[test]
    fn unknown_package_lists_choices() {
        let error = select(&workspace(), &context(Some("missing"), None)).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("missing"), "{text}");
        assert!(text.contains("api, worker"), "{text}");
    }

    #[test]
    fn unknown_binary_lists_choices() {
        let error = select(&workspace(), &context(Some("api"), Some("missing"))).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("missing"), "{text}");
        assert!(text.contains("migrate, serve"), "{text}");
    }

    #[test]
    fn current_package_is_inferred_from_the_working_directory() {
        let mut selected_context = context(None, None);
        selected_context.current_dir = PathBuf::from("/workspace/worker/src");
        let selected = select(&workspace(), &selected_context).unwrap();
        assert_eq!(selected.package_name, "worker");
    }

    #[test]
    fn a_package_manifest_path_selects_that_package() {
        let mut selected_context = context(None, None);
        selected_context.manifest_path = Some(PathBuf::from("/workspace/api/Cargo.toml"));
        let selected = select(&workspace(), &selected_context).unwrap();
        assert_eq!(selected.package_name, "api");
    }

    #[test]
    fn a_sole_default_member_is_inferred() {
        let metadata: Metadata = serde_json::from_str(
            &WORKSPACE.replace(
                "\"path+file:///workspace/api#0.1.0\",\n        \"path+file:///workspace/worker#0.1.0\"\n      ],\n      \"resolve\"",
                "\"path+file:///workspace/worker#0.1.0\"\n      ],\n      \"resolve\"",
            ),
        )
        .unwrap();
        let selected = select(&metadata, &context(None, None)).unwrap();
        assert_eq!(selected.package_name, "worker");
    }

    #[test]
    fn multiple_uninferred_binaries_require_bin() {
        let mut metadata = workspace();
        metadata.packages[0].default_run = None;
        metadata.packages[0].name = cargo_metadata::PackageName::new("frontend".to_owned());
        let error = select(&metadata, &context(Some("frontend"), None)).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("--bin"), "{text}");
        assert!(text.contains("migrate, serve"), "{text}");
    }

    #[test]
    fn a_sole_binary_is_inferred() {
        let mut metadata = workspace();
        metadata.packages[1].name = cargo_metadata::PackageName::new("jobs".to_owned());
        let selected = select(&metadata, &context(Some("jobs"), None)).unwrap();
        assert_eq!(selected.binary_name, "worker");
    }
}
