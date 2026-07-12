//! Webhook payload types for the `pull_request` event.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct User {
    pub id: i64,
    pub avatar_url: String,
    pub email: Option<String>,
    pub login: Option<String>,
    pub name: Option<String>,
    pub url: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GitRef {
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PullRequest {
    pub id: i64,
    pub additions: i32,
    pub assignee: Option<User>,
    pub assignees: Option<Vec<User>>,
    pub body: Option<String>,
    pub changed_files: i32,
    pub closed_at: Option<DateTime<Utc>>,
    pub comments: i32,
    pub comments_url: String,
    pub commits: i32,
    pub commits_url: String,
    pub created_at: DateTime<Utc>,
    pub deletions: i32,
    pub diff_url: String,
    pub draft: Option<bool>,
    pub head: GitRef,
    pub html_url: String,
    pub issue_url: String,
    pub locked: bool,
    pub merged: bool,
    pub merged_at: Option<DateTime<Utc>>,
    pub merged_by: Option<User>,
    pub number: i32,
    pub review_comment_url: String,
    pub review_comments: i32,
    pub review_comments_url: String,
    pub state: String, // open / closed
    pub statuses_url: String,
    pub title: String,
    pub updated_at: DateTime<Utc>,
    pub url: String,
    pub user: User,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Repository {
    pub id: i64,
    pub clone_url: String,
    pub full_name: String,
    pub name: String,
    pub owner: User,
    pub private: bool,
}

impl Repository {
    /// Splits `full_name` ("owner/name") for API calls.
    pub fn owner_and_name(&self) -> Option<(&str, &str)> {
        self.full_name.split_once('/')
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PullRequestOpened {
    pub number: i32,
    pub pull_request: PullRequest,
    pub repository: Repository,
    pub sender: User,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PullRequestSync {
    pub after: String,
    pub before: String,
    pub number: i32,
    pub pull_request: PullRequest,
    pub repository: Repository,
    pub sender: User,
}

/// GitHub discriminates `pull_request` webhooks by the top-level `action`
/// field, so the enum is internally tagged on it. Actions we don't handle
/// deserialize to `Unsupported` instead of failing the request.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PullRequestWH {
    Opened(PullRequestOpened),
    Synchronize(PullRequestSync),
    #[serde(other)]
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> serde_json::Value {
        serde_json::json!({
            "id": 1,
            "avatar_url": "https://avatars.example/u/1",
            "email": null,
            "login": "octocat",
            "name": null,
            "url": "https://api.github.com/users/octocat"
        })
    }

    fn pull_request() -> serde_json::Value {
        serde_json::json!({
            "additions": 10,
            "assignee": null,
            "assignees": [],
            "body": "a body",
            "changed_files": 2,
            "closed_at": null,
            "comments": 0,
            "comments_url": "https://api.github.com/c",
            "commits": 1,
            "commits_url": "https://api.github.com/commits",
            "created_at": "2026-07-12T07:15:43Z",
            "deletions": 1,
            "diff_url": "https://github.com/o/r/pull/7.diff",
            "draft": false,
            "head": { "ref": "feature", "sha": "headsha123" },
            "html_url": "https://github.com/o/r/pull/7",
            "id": 42,
            "issue_url": "https://api.github.com/issues/7",
            "locked": false,
            "mergeable": true,
            "merged": false,
            "merged_at": null,
            "merged_by": null,
            "number": 7,
            "review_comment_url": "https://api.github.com/rc",
            "review_comments": 0,
            "review_comments_url": "https://api.github.com/rcs",
            "state": "open",
            "statuses_url": "https://api.github.com/statuses",
            "title": "a title",
            "updated_at": "2026-07-12T00:00:00Z",
            "url": "https://api.github.com/pulls/7",
            "user": user()
        })
    }

    fn repository() -> serde_json::Value {
        serde_json::json!({
            "clone_url": "https://github.com/octocat/repo.git",
            "full_name": "octocat/repo",
            "id": 1,
            "name": "repo",
            "owner": user(),
            "private": false
        })
    }

    pub(crate) fn payload(action: &str, extra: serde_json::Value) -> String {
        let mut base = serde_json::json!({
            "action": action,
            "number": 7,
            "pull_request": pull_request(),
            "repository": repository(),
            "sender": user()
        });
        let obj = base.as_object_mut().unwrap();
        for (k, v) in extra.as_object().unwrap() {
            obj.insert(k.clone(), v.clone());
        }
        base.to_string()
    }

    #[test]
    fn resolves_opened() {
        let wh: PullRequestWH =
            serde_json::from_str(&payload("opened", serde_json::json!({}))).unwrap();

        match wh {
            PullRequestWH::Opened(opened) => {
                assert_eq!(opened.number, 7);
                assert_eq!(opened.pull_request.id, 42);
                assert_eq!(opened.pull_request.head.sha, "headsha123");
                assert_eq!(opened.repository.full_name, "octocat/repo");
                assert_eq!(
                    opened.repository.owner_and_name(),
                    Some(("octocat", "repo"))
                );
            }
            other => panic!("expected Opened, got {other:?}"),
        }
    }

    #[test]
    fn resolves_synchronize() {
        let extra = serde_json::json!({ "before": "aaa111", "after": "bbb222" });
        let wh: PullRequestWH = serde_json::from_str(&payload("synchronize", extra)).unwrap();

        match wh {
            PullRequestWH::Synchronize(sync) => {
                assert_eq!(sync.before, "aaa111");
                assert_eq!(sync.after, "bbb222");
                assert_eq!(sync.number, 7);
            }
            other => panic!("expected Synchronize, got {other:?}"),
        }
    }

    #[test]
    fn unknown_action_resolves_unsupported() {
        let wh: PullRequestWH =
            serde_json::from_str(&payload("labeled", serde_json::json!({}))).unwrap();

        assert!(matches!(wh, PullRequestWH::Unsupported));
    }

    #[test]
    fn parses_timestamps_into_dates() {
        let wh: PullRequestWH =
            serde_json::from_str(&payload("opened", serde_json::json!({}))).unwrap();

        let PullRequestWH::Opened(opened) = wh else {
            panic!("expected Opened");
        };
        let pr = opened.pull_request;

        assert_eq!(
            pr.created_at,
            "2026-07-12T07:15:43Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(pr.created_at.date_naive().to_string(), "2026-07-12");
        // open PR: GitHub sends null for these
        assert!(pr.closed_at.is_none());
        assert!(pr.merged_at.is_none());
    }

    #[test]
    fn rejects_malformed_timestamp() {
        let mut body: serde_json::Value =
            serde_json::from_str(&payload("opened", serde_json::json!({}))).unwrap();
        body["pull_request"]["created_at"] = serde_json::json!("not a date");

        assert!(serde_json::from_value::<PullRequestWH>(body).is_err());
    }

    #[test]
    fn synchronize_without_before_after_is_rejected() {
        let json = payload("synchronize", serde_json::json!({}));

        assert!(serde_json::from_str::<PullRequestWH>(&json).is_err());
    }
}
