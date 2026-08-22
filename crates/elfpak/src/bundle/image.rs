//! Resolve OCI image metadata from CLI and configuration values.

use crate::{cli::BundleArgs, config::Config};
use elfpak_core::{Error, OciImageConfig};
use std::{collections::BTreeMap, path::Path};

/// Whether image metadata was named on the command line, which is a statement
/// about *this* invocation. A configured `[image]` table is a standing
/// declaration that need not apply to a rootfs-only build, so it is not
/// counted here and continues to be ignored when no image is produced.
pub(crate) fn was_requested_on_the_command_line(args: &BundleArgs) -> bool {
    args.image_tag.is_some()
        || !args.entrypoint.is_empty()
        || !args.cmd.is_empty()
        || args.working_dir.is_some()
        || !args.env.is_empty()
        || !args.label.is_empty()
}

pub(crate) fn resolve(args: &BundleArgs, config: &Config) -> anyhow::Result<OciImageConfig> {
    let tag = args
        .image_tag
        .clone()
        .or_else(|| config.image.tag.clone())
        .unwrap_or_else(|| "latest".to_string());
    let entrypoint = replace_collection(&args.entrypoint, &config.image.entrypoint);
    let cmd = replace_collection(&args.cmd, &config.image.cmd);
    let working_dir = args
        .working_dir
        .as_deref()
        .or(config.image.working_dir.as_deref())
        .map(unicode_path)
        .transpose()?;
    let env = if args.env.is_empty() {
        config
            .image
            .env
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect()
    } else {
        args.env.clone()
    };
    let labels = if args.label.is_empty() {
        config.image.labels.clone()
    } else {
        parse_labels(&args.label)?
    };

    Ok(OciImageConfig {
        tag,
        entrypoint,
        cmd,
        working_dir,
        env,
        labels,
    })
}

fn replace_collection(cli: &[String], configured: &[String]) -> Vec<String> {
    if cli.is_empty() {
        configured.to_vec()
    } else {
        cli.to_vec()
    }
}

fn unicode_path(path: &Path) -> anyhow::Result<String> {
    path.to_str().map(str::to_string).ok_or_else(|| {
        Error::Config {
            message: format!(
                "OCI working directory `{}` is not valid Unicode",
                path.display()
            ),
        }
        .into()
    })
}

fn parse_labels(values: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut labels = BTreeMap::new();
    for value in values {
        let (key, value) = value.split_once('=').ok_or_else(|| Error::Config {
            message: format!("invalid --label `{value}` (expected KEY=VALUE)"),
        })?;
        if labels.insert(key.to_string(), value.to_string()).is_some() {
            return Err(Error::Config {
                message: format!("duplicate --label key `{key}`"),
            }
            .into());
        }
    }
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;
    use std::path::PathBuf;

    fn args(values: &[&str]) -> BundleArgs {
        let cli = Cli::try_parse_from(
            [
                "elfpak",
                "bundle",
                "/bin/app",
                "--oci-layout",
                "image",
                "--dry-run",
            ]
            .into_iter()
            .chain(values.iter().copied()),
        )
        .unwrap();
        match cli.command {
            Command::Bundle(bundle) => bundle.into_bundle(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn cli_image_values_replace_every_configured_value() {
        let config = Config::parse(
            r#"
[image]
tag = "configured"
entrypoint = ["/configured"]
cmd = ["configured-command"]
working_dir = "/configured"
env = { CONFIGURED = "true" }
labels = { configured = "true" }
"#,
        )
        .unwrap();
        let args = args(&[
            "--image-tag",
            "cli",
            "--entrypoint",
            "/cli",
            "--entrypoint",
            "argument",
            "--cmd",
            "cli-command",
            "--working-dir",
            "/cli-dir",
            "--env",
            "CLI=true",
            "--label",
            "cli=true",
        ]);

        let image = resolve(&args, &config).unwrap();
        assert_eq!(image.tag, "cli");
        assert_eq!(image.entrypoint, ["/cli", "argument"]);
        assert_eq!(image.cmd, ["cli-command"]);
        assert_eq!(image.working_dir, Some("/cli-dir".to_string()));
        assert_eq!(image.env, ["CLI=true"]);
        assert_eq!(
            image.labels,
            BTreeMap::from([("cli".to_string(), "true".to_string())])
        );
    }

    #[test]
    fn configured_maps_resolve_in_sorted_order() {
        let config = Config::parse(
            "[image]\nenv = { ZED = 'last', ALPHA = 'first' }\nlabels = { zed = 'last', alpha = 'first' }\n",
        )
        .unwrap();
        let image = resolve(&args(&[]), &config).unwrap();
        assert_eq!(image.env, ["ALPHA=first", "ZED=last"]);
        assert_eq!(image.labels.keys().collect::<Vec<_>>(), ["alpha", "zed"]);
    }

    #[test]
    fn cli_labels_require_unique_key_value_pairs() {
        let invalid =
            resolve(&args(&["--label", "missing-separator"]), &Config::default()).unwrap_err();
        assert!(invalid.to_string().contains("KEY=VALUE"));

        let duplicate = resolve(
            &args(&["--label", "key=one", "--label", "key=two"]),
            &Config::default(),
        )
        .unwrap_err();
        assert!(duplicate.to_string().contains("duplicate"));
    }

    #[test]
    fn non_unicode_working_directories_are_rejected() {
        use std::os::unix::ffi::OsStringExt;

        let mut args = args(&[]);
        args.working_dir = Some(PathBuf::from(std::ffi::OsString::from_vec(vec![0xff])));
        let error = resolve(&args, &Config::default()).unwrap_err();
        assert!(error.to_string().contains("Unicode"));
    }
}
