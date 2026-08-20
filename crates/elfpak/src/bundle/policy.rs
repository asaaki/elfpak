//! Runtime and dependency policy, assembled from the preset, the configuration
//! file and the flags, in that order of precedence.

use crate::cli::BundleArgs;
use elfpak_core::{CachePolicy, DependencyPolicy, Preset, RuntimePolicy, UserSpec, config::Config};

/// The preset, then every feature the caller switched on its own.
pub(crate) fn runtime_policy(
    args: &BundleArgs,
    config: &Config,
    preset: Option<Preset>,
) -> anyhow::Result<RuntimePolicy> {
    let mut policy = RuntimePolicy::from_preset(preset.unwrap_or(Preset::Minimal));

    override_flag(
        &mut policy.ca_certificates,
        args.ca_certificates.or(config.runtime.ca_certificates),
    );
    override_flag(&mut policy.tmp, args.tmp.or(config.runtime.tmp));
    override_flag(
        &mut policy.passwd_group,
        args.passwd_group.or(config.runtime.passwd_group),
    );
    override_flag(
        &mut policy.nsswitch,
        args.nsswitch.or(config.runtime.nsswitch),
    );
    override_flag(&mut policy.tzdata, args.tzdata.or(config.runtime.tzdata));

    policy.ld_so_cache = CachePolicy::from_flag(args.ld_so_cache.or(config.runtime.ld_so_cache));
    if let Some(user) = args.user.clone().or_else(|| config.runtime.user.clone()) {
        policy.user = Some(UserSpec::parse(&user)?);
    }
    // A repeated `--include` replaces the configured list rather than adding to
    // it, so that a command line can always be read on its own.
    policy.includes = if args.includes.is_empty() {
        config.include.paths.clone()
    } else {
        args.includes.clone()
    };

    Ok(policy)
}

/// `None` leaves the preset's answer in place; a flag or config value wins.
fn override_flag(target: &mut bool, value: Option<bool>) {
    if let Some(value) = value {
        *target = value;
    }
}

pub(crate) fn dependency_policy(args: &BundleArgs, config: &Config) -> DependencyPolicy {
    let allow = if args.allow_library.is_empty() {
        config.dependencies.allow.clone()
    } else {
        Some(args.allow_library.clone())
    };
    match allow {
        Some(list) => DependencyPolicy::allow_list(list),
        None => DependencyPolicy::allow_all(),
    }
}
