//! Per-PR review state, persisted as JSON.
//!
//! Tracking exists for idempotency: the same head sha is never reviewed
//! twice, and concurrent/redelivered webhooks are deduplicated. The
//! attempt/review caps are the safeguard against review loops. Open review
//! comments are NOT tracked here -- GitHub's unresolved review threads are
//! the source of truth for those (see `GhClient::open_guardian_comments`).

use std::{collections::HashMap, io, path::PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::ReviewLimits;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewStatus {
    InProgress,
    Reviewed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrState {
    pub head_sha: String,
    pub status: ReviewStatus,
    /// Review attempts on the current head sha (failures included).
    pub attempts_on_sha: u32,
    /// Completed reviews over the PR's lifetime.
    pub total_reviews: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BeginReview {
    Proceed,
    /// Same head sha already reviewed or currently in flight.
    AlreadyHandled,
    /// Same head sha failed too many times.
    AttemptsExhausted,
    /// The PR hit its lifetime review cap.
    ReviewCapReached,
}

pub struct StateStore {
    path: PathBuf,
    limits: ReviewLimits,
    prs: Mutex<HashMap<String, PrState>>,
}

impl StateStore {
    pub fn load(path: PathBuf, limits: ReviewLimits) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let prs = match std::fs::read(&path) {
            Ok(raw) => serde_json::from_slice(&raw)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            path,
            limits,
            prs: Mutex::new(prs),
        })
    }

    /// Atomically claims the review of `head_sha` for `key`. Only a
    /// `Proceed` answer entitles the caller to run a review.
    pub async fn begin_review(&self, key: &str, head_sha: &str) -> BeginReview {
        let mut prs = self.prs.lock().await;
        match prs.get_mut(key) {
            Some(state) if state.head_sha == head_sha => match state.status {
                ReviewStatus::InProgress | ReviewStatus::Reviewed => BeginReview::AlreadyHandled,
                ReviewStatus::Failed if state.attempts_on_sha >= self.limits.max_attempts_per_sha => {
                    BeginReview::AttemptsExhausted
                }
                ReviewStatus::Failed => {
                    state.attempts_on_sha += 1;
                    state.status = ReviewStatus::InProgress;
                    self.persist(&prs);
                    BeginReview::Proceed
                }
            },
            Some(state) if state.total_reviews >= self.limits.max_reviews_per_pr => {
                BeginReview::ReviewCapReached
            }
            existing => {
                let total_reviews = existing.map(|s| s.total_reviews).unwrap_or_default();
                prs.insert(
                    key.to_owned(),
                    PrState {
                        head_sha: head_sha.to_owned(),
                        status: ReviewStatus::InProgress,
                        attempts_on_sha: 1,
                        total_reviews,
                    },
                );
                self.persist(&prs);
                BeginReview::Proceed
            }
        }
    }

    pub async fn finish_review(&self, key: &str) {
        let mut prs = self.prs.lock().await;
        if let Some(state) = prs.get_mut(key) {
            state.status = ReviewStatus::Reviewed;
            state.total_reviews += 1;
        }
        self.persist(&prs);
    }

    pub async fn mark_failed(&self, key: &str) {
        let mut prs = self.prs.lock().await;
        if let Some(state) = prs.get_mut(key) {
            state.status = ReviewStatus::Failed;
        }
        self.persist(&prs);
    }

    fn persist(&self, prs: &HashMap<String, PrState>) {
        let result = serde_json::to_vec_pretty(prs)
            .map_err(io::Error::other)
            .and_then(|raw| std::fs::write(&self.path, raw));
        if let Err(e) = result {
            tracing::error!(path = %self.path.display(), error = %e, "failed to persist state");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &tempfile::TempDir) -> StateStore {
        StateStore::load(dir.path().join("state.json"), ReviewLimits::default()).unwrap()
    }

    #[tokio::test]
    async fn same_sha_is_processed_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);

        assert_eq!(store.begin_review("r#1", "aaa").await, BeginReview::Proceed);
        // in flight
        assert_eq!(
            store.begin_review("r#1", "aaa").await,
            BeginReview::AlreadyHandled
        );
        store.finish_review("r#1").await;
        // already reviewed
        assert_eq!(
            store.begin_review("r#1", "aaa").await,
            BeginReview::AlreadyHandled
        );
        // new push is allowed
        assert_eq!(store.begin_review("r#1", "bbb").await, BeginReview::Proceed);
    }

    #[tokio::test]
    async fn failed_sha_retries_up_to_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);

        for _ in 0..3 {
            assert_eq!(store.begin_review("r#1", "aaa").await, BeginReview::Proceed);
            store.mark_failed("r#1").await;
        }
        assert_eq!(
            store.begin_review("r#1", "aaa").await,
            BeginReview::AttemptsExhausted
        );
        // a new push resets the attempt budget
        assert_eq!(store.begin_review("r#1", "bbb").await, BeginReview::Proceed);
    }

    #[tokio::test]
    async fn lifetime_review_cap_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::load(
            dir.path().join("state.json"),
            ReviewLimits {
                max_attempts_per_sha: 3,
                max_reviews_per_pr: 2,
            },
        )
        .unwrap();

        for sha in ["aaa", "bbb"] {
            assert_eq!(store.begin_review("r#1", sha).await, BeginReview::Proceed);
            store.finish_review("r#1").await;
        }
        assert_eq!(
            store.begin_review("r#1", "ccc").await,
            BeginReview::ReviewCapReached
        );
    }

    #[tokio::test]
    async fn load_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dirs/state.json");

        let store = StateStore::load(path.clone(), ReviewLimits::default()).unwrap();
        store.begin_review("r#1", "aaa").await;

        assert!(path.exists());
    }

    #[tokio::test]
    async fn state_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        {
            let store = StateStore::load(path.clone(), ReviewLimits::default()).unwrap();
            store.begin_review("r#1", "aaa").await;
            store.finish_review("r#1").await;
        }

        let store = StateStore::load(path, ReviewLimits::default()).unwrap();
        assert_eq!(
            store.begin_review("r#1", "aaa").await,
            BeginReview::AlreadyHandled
        );
    }

    #[tokio::test]
    async fn state_files_with_legacy_comments_still_load() {
        // .state.json written before comment tracking moved to GitHub
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "r#1": {
                    "head_sha": "aaa",
                    "status": "Reviewed",
                    "attempts_on_sha": 1,
                    "total_reviews": 1,
                    "comments": [{
                        "comment_id": 7,
                        "comment": {
                            "severity": "design",
                            "text": "t",
                            "file": "src/a.rs",
                            "lines": {"start": 1, "end": 2}
                        }
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        let store = StateStore::load(path, ReviewLimits::default()).unwrap();
        assert_eq!(
            store.begin_review("r#1", "aaa").await,
            BeginReview::AlreadyHandled
        );
    }
}
