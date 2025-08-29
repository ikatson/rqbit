use anyhow::Context;
use tracing::{info, warn};

pub fn get_configuration_directory_linux(
    application: &str,
) -> anyhow::Result<directories::ProjectDirs> {
    // https://github.com/ikatson/rqbit/issues/252
    // On Linux, "com.rqbit" isn't used resulting in weird folder names like "session".
    // The code below migrates from old configuration to new.
    let legacy_dir =
        directories::ProjectDirs::from("com", "rqbit", application).with_context(|| {
            format!("cannot determine project directory for com.rqbit.{application}")
        })?;
    let new_dir =
        directories::ProjectDirs::from("com", "rqbit", &format!("com.rqbit.{application}"))
            .with_context(|| {
                format!("cannot determine project directory for com.rqbit.{application}")
            })?;
    for (old, new) in [
        (legacy_dir.cache_dir(), new_dir.cache_dir()),
        (legacy_dir.config_dir(), new_dir.config_dir()),
        (legacy_dir.data_dir(), new_dir.data_dir()),
    ] {
        match (old.exists(), new.exists()) {
            (true, true) => {
                warn!(
                    ?old,
                    ?new,
                    "can't migrate configuration as both directories exist, not sure what to do"
                )
            }
            (true, false) => {
                info!(
                    ?old,
                    ?new,
                    "migrating configuration directories as rqbit was upgraded"
                );
                if let Err(e) = std::fs::rename(old, new) {
                    warn!(?old, ?new, "error migrating: {e:#}");
                }
            }
            // In these cases, nothing to migrate, so do nothing.
            (false, true) => {}
            (false, false) => {}
        }
    }
    Ok(new_dir)
}

pub fn get_configuration_directory(application: &str) -> anyhow::Result<directories::ProjectDirs> {
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        get_configuration_directory_linux(application)
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        directories::ProjectDirs::from("com", "rqbit", application).with_context(|| {
            format!("cannot determine project directory for com.rqbit.{application}")
        })
    }
}
