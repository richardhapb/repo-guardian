#[macro_use]
extern crate rocket;

use std::{collections::HashMap, fmt::Display};

use chrono::{DateTime, Utc};
use rocket::serde::{Deserialize, Serialize, json::Json};

struct Config {
    auto_merge: bool,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(crate = "rocket::serde")]
struct Comment {
    text: String,
    file: String,
    lines: LineRange,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(crate = "rocket::serde")]
struct LineRange {
    start: usize,
    end: usize,
}

impl Display for LineRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}:{}", self.start, self.end)
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(crate = "rocket::serde")]
struct PullRequest {
    id: i64,
    additions: i32,
    assignee: Option<User>,
    assignees: Option<Vec<User>>,
    body: Option<String>,
    changed_files: i32,
    closed_at: Option<DateTime<Utc>>,
    comments: i32,
    comments_url: String,
    commits: i32,
    commits_url: String,
    created_at: DateTime<Utc>,
    deletions: i32,
    diff_url: String,
    draft: Option<bool>,
    html_url: String,
    issue_url: String,
    locked: bool,
    merged: bool,
    merged_at: Option<DateTime<Utc>>,
    merged_by: Option<User>,
    number: i32,
    review_comment_url: String,
    review_comments: i32,
    review_comments_url: String,
    state: String, // open / closed
    statuses_url: String,
    title: String,
    updated_at: DateTime<Utc>,
    url: String,
    user: User,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(crate = "rocket::serde")]
struct User {
    id: i64,
    avatar_url: String,
    email: Option<String>,
    login: Option<String>,
    name: Option<String>,
    url: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(crate = "rocket::serde")]
struct Repository {
    id: i64,
    full_name: String,
    name: String,
    owner: User,
    private: bool,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(crate = "rocket::serde")]
struct PullRequestOpened {
    number: i32,
    pull_request: PullRequest,
    repository: Repository,
    sender: User,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(crate = "rocket::serde")]
struct PullRequestSync {
    after: String,
    before: String,
    number: i32,
    pull_request: PullRequest,
    repository: Repository,
    sender: User,
}

/// GitHub discriminates `pull_request` webhooks by the top-level `action`
/// field, so the enum is internally tagged on it. Actions we don't handle
/// deserialize to `Unsupported` instead of failing the request.
#[derive(Serialize, Deserialize, Debug)]
#[serde(crate = "rocket::serde", tag = "action", rename_all = "snake_case")]
enum PullRequestWH {
    Opened(PullRequestOpened),
    Synchronize(PullRequestSync),
    #[serde(other)]
    Unsupported,
}

#[launch]
fn rocket() -> _ {
    rocket::build().mount("/", routes![webhook_gh])
}

#[post("/webhooks/gh", data = "<payload>")]
fn webhook_gh(payload: Json<PullRequestWH>) {
    dbg!(payload);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocket::serde::json::serde_json;

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
            "full_name": "octocat/repo",
            "id": 1,
            "name": "repo",
            "owner": user(),
            "private": false
        })
    }

    fn payload(action: &str, extra: serde_json::Value) -> String {
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
                assert_eq!(opened.repository.full_name, "octocat/repo");
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
