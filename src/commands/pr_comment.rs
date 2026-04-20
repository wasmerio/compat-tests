use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct PrCommentArgs {}

pub fn pr_comment(args: PrCommentArgs) -> Result<()> {
    tracing::info!("pr-comment");
    let _ = args;
    // TODO: render summary markdown.
    Ok(())
}
