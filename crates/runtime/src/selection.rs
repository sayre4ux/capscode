use capscode_core::{RuntimeAdapter, RuntimeError, RuntimePref};

use crate::{DockerAdapter, OrbStackAdapter};

pub async fn detect(pref: RuntimePref) -> Result<Box<dyn RuntimeAdapter>, RuntimeError> {
    match pref {
        RuntimePref::Docker => select_docker().await,
        RuntimePref::OrbStack => select_orbstack().await,
        RuntimePref::Auto => {
            if let Ok(adapter) = select_orbstack().await {
                return Ok(adapter);
            }
            if let Ok(adapter) = select_docker().await {
                return Ok(adapter);
            }
            Err(RuntimeError::NoRuntime)
        }
    }
}

async fn select_docker() -> Result<Box<dyn RuntimeAdapter>, RuntimeError> {
    let adapter = DockerAdapter::new()?;
    adapter.preflight().await?;
    Ok(Box::new(adapter))
}

async fn select_orbstack() -> Result<Box<dyn RuntimeAdapter>, RuntimeError> {
    let adapter = OrbStackAdapter::new()?;
    adapter.preflight().await?;
    Ok(Box::new(adapter))
}
