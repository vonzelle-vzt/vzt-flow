use anyhow::Result;

pub fn download(model: &str, force: bool) -> Result<()> {
    match model {
        "parakeet-v3" => {
            flow_core::models::download_parakeet_v3(force)?;
            Ok(())
        }
        "cleanup" => {
            flow_core::models::download_cleanup_model(force)?;
            Ok(())
        }
        other => anyhow::bail!("unknown model '{other}'. Supported: parakeet-v3, cleanup"),
    }
}
