//! `elfpak inspect`: analyze an executable and print its closure, copying
//! nothing.

use crate::{
    cli::InspectArgs,
    render::{self, Verbosity},
};
use elfpak_core::{Manifest, Planner, SourceRoot};

pub(crate) fn run(args: &InspectArgs, verbosity: Verbosity) -> anyhow::Result<()> {
    let root = SourceRoot::new(&args.root);
    let plan = Planner::new(root, &args.binary)
        .library_paths(args.library_paths.clone())
        .plan()?;

    if args.json {
        let manifest = Manifest::from_plan(&plan, &args.root, None);
        println!("{}", manifest.to_json());
        return Ok(());
    }

    verbosity.print(|out| render::inspect(out, &args.binary, &plan));
    Ok(())
}
