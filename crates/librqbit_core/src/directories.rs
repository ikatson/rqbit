use anyhow::Context;

pub fn get_configuration_directory(application: &str) -> anyhow::Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("com", "rqbit", application)
        .with_context(|| format!("cannot determine project directory for com.rqbit.{application}"))
}
