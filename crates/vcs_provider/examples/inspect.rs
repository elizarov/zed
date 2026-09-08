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
        Ok(())
    })
}
