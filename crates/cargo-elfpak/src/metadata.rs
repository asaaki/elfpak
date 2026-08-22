use anyhow::{Context, Result, bail};
use cargo_metadata::{Metadata, Package, PackageId, Target};
use std::path::{Path, PathBuf};

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
    pub(crate) manifest_path: Option<PathBuf>,
    pub(crate) current_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct Selection {
    pub(crate) package_id: PackageId,
    pub(crate) package_name: String,
    pub(crate) binary_name: String,
}

pub(crate) fn select(metadata: &Metadata, context: &SelectionContext) -> Result<Selection> {
    let package = select_package(metadata, context)?;
    let binary = select_binary(package, context.binary.as_deref())?;

    Ok(Selection {
        package_id: package.id.clone(),
        package_name: package.name.to_string(),
        binary_name: binary.name.clone(),
    })
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

    if let Some(manifest_path) = context.manifest_path.as_deref() {
        let manifest_path = absolute_path(manifest_path, &context.current_dir);
        if let Some(package) = workspace_packages
            .iter()
            .copied()
            .find(|package| package.manifest_path.as_std_path() == manifest_path)
        {
            return Ok(package);
        }
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
    let mut binaries: Vec<_> = package
        .targets
        .iter()
        .filter(|target| target.is_bin())
        .collect();
    binaries.sort_by(|left, right| left.name.cmp(&right.name));
    if binaries.is_empty() {
        bail!("package `{}` has no binary targets", package.name);
    }

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

fn absolute_path(path: &Path, current_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    }
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
    use super::{SelectionContext, select};
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
            manifest_path: None,
            current_dir: PathBuf::from("/workspace"),
        }
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
