use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct PublishArgs {
    /// Results branch name (typically the wasmer ref).
    #[arg(long)]
    pub branch: String,
}

pub fn publish(args: PublishArgs) -> Result<()> {
    tracing::info!(branch = %args.branch, "publish");
    let _ = args;
    // TODO: commit results/<lang>/… into the branch.
    Ok(())
}
