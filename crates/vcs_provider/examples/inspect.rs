use anyhow::{Context as _, Result};
use std::{
    path::Path,
    time::{Duration, Instant},
};

fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let root = arguments
        .next()
        .context("usage: inspect ROOT RELATIVE_PATH COMMAND [ARG ...]")?;
    let path = arguments.next().context("missing relative path")?;
    let command = arguments.next().context("missing provider command")?;
    let arguments = arguments.collect::<Vec<_>>();
    smol::block_on(async {
        let start = Instant::now();
        let client = vcs_provider::Client::start(
            &command,
            &arguments,
            &Default::default(),
            Path::new(&root),
            Duration::from_secs(60),
        )
        .await?;
        println!(
            "{}: {} status entries; initialized in {:?}",
            client.repository.label,
            client.snapshot().changes.len(),
            start.elapsed()
        );
        let start = Instant::now();
        let contents = client
            .contents(&[path.clone(), path], &[false, true])
            .await?;
        println!(
            "Baseline byte lengths: {:?}; read in {:?}",
            contents
                .iter()
                .map(|content| content.as_ref().map(Vec::len))
                .collect::<Vec<_>>(),
            start.elapsed()
        );
        let start = Instant::now();
        client.refresh().await?;
        println!("Status refreshed in {:?}", start.elapsed());
        if client.capabilities.history {
            let revision = client
                .snapshot()
                .revision
                .context("missing current revision")?;
            let history = client.history(&revision, None, 5).await?;
            println!(
                "History: {} commits; older commits: {}",
                history.commits.len(),
                history.has_more
            );
            if let Some(commit) = history.commits.first() {
                let details = client.commit_details(&commit.id).await?;
                let changes = client.commit_changes(&commit.id).await?;
                println!(
                    "Newest scoped commit: {} message bytes, {} changed files",
                    details.message.len(),
                    changes.len()
                );
                if let Some(change) = changes.first() {
                    for reference in [&change.base, &change.target].into_iter().flatten() {
                        println!(
                            "Historical content bytes: {}",
                            client.read_content(reference).await?.len()
                        );
                    }
                }
            }
        }

        Ok(())
    })
}
