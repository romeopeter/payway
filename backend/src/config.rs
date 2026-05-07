use anyhow::Context;

pub struct Config {
    pub database_url: String,
    pub port: u16,
    pub webhook_secret: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            database_url: env_required("DATABASE_URL")?,
            port: env_optional("PORT", "8080")
                .parse()
                .context("PORT must be a u16")?,
            webhook_secret: env_required("WEBHOOK_SECRET")?,
        })
    }
}

fn env_required(key: &str) -> anyhow::Result<String> {
    std::env::var(key).with_context(|| format!("required env var {key} is not set"))
}

fn env_optional(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
