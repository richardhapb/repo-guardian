#[macro_use]
extern crate rocket;

mod config;
mod github;
mod guardian;
mod pipeline;
mod repos;
mod state;
mod webhook;

use std::{path::Path, sync::Arc};

use config::Config;
use github::GhClient;
use guardian::Guardian;
use state::StateStore;
use webhook::WebhookSecret;

pub struct App {
    pub config: Config,
    pub gh: GhClient,
    pub guardian: Guardian,
    pub store: StateStore,
    /// Login of the account the GitHub token belongs to. Its own PRs can't
    /// be approved (GitHub restriction), so they get a comment review
    /// instead -- and a merge when `auto_merge` is on.
    pub username: Option<String>,
}

#[launch]
async fn rocket() -> _ {
    // RUST_LOG overrides, e.g. RUST_LOG=repo_guardian=debug
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::load(Path::new("config.toml")).unwrap_or_default();
    let secret = std::env::var("GITHUB_WEBHOOK_SECRET").expect("GITHUB_WEBHOOK_SECRET must be set");

    // scoped: the builder is not Send and must not live across an await
    let octocrab = {
        let mut builder = octocrab::Octocrab::builder();
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            builder = builder.personal_token(token);
        }
        builder.build().expect("build GitHub client")
    };

    // Fail at boot, not mid-review, when the configured paths are unusable.
    std::fs::create_dir_all(&config.repos_path).unwrap_or_else(|e| {
        panic!(
            "repos_path {} is not writable: {e}",
            config.repos_path.display()
        )
    });
    let username = match octocrab.current().user().await {
        Ok(user) => {
            tracing::info!(username = %user.login, "authenticated to GitHub");
            Some(user.login)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not resolve the authenticated user; own-PR detection disabled"
            );
            None
        }
    };

    let store = StateStore::load(config.state_path(), config.limits).expect("load state");
    let app = Arc::new(App {
        gh: GhClient::new(octocrab),
        guardian: Guardian::new(),
        store,
        config,
        username,
    });

    rocket::build()
        .manage(app)
        .manage(WebhookSecret::new(secret))
        .mount("/", routes![webhook::webhook_gh])
}
