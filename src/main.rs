fn main() -> anyhow::Result<()> {
    let cli = std::thread::Builder::new()
        .name("remi-cat-cli".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(remi_cat::run_cli())
        })?;
    match cli.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
