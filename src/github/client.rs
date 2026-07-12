//! Thin wrapper over octocrab for the operations the pipeline needs.

use octocrab::{Octocrab, models::pulls::Review};
use serde_json::json;

use crate::guardian::{Comment, LineRange, Severity};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Identifies comments posted by us; the pre-review thread fetch keys on it.
pub const MARKER: &str = "[Repo Guardian]";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    /// Neutral review; the only kind GitHub accepts on one's own PR.
    Comment,
}

impl ReviewVerdict {
    fn as_str(self) -> &'static str {
        match self {
            ReviewVerdict::Approve => "APPROVE",
            ReviewVerdict::RequestChanges => "REQUEST_CHANGES",
            ReviewVerdict::Comment => "COMMENT",
        }
    }
}

/// Body posted to GitHub for an inline comment: marker and severity badge
/// as a heading line, then the finding.
pub fn comment_body(comment: &Comment) -> String {
    format!("{MARKER} {}\n\n{}", comment.severity.badge(), comment.text)
}

/// Inverse of [`comment_body`]. Bodies starting with a bare severity badge
/// (comments posted before the marker existed) also parse.
fn parse_comment_body(body: &str) -> Option<(Severity, String)> {
    let rest = body
        .strip_prefix(MARKER)
        .map(str::trim_start)
        .unwrap_or(body);
    let severity = [Severity::Bug, Severity::Design, Severity::Nit]
        .into_iter()
        .find(|s| rest.starts_with(s.badge()))?;
    let text = rest[severity.badge().len()..].trim_start().to_owned();
    Some((severity, text))
}

/// An unresolved Guardian comment fetched from the PR's review threads.
/// GitHub is the source of truth for these, so they survive restarts and
/// lost state; `thread_id` is what the resolve mutation needs.
#[derive(Clone, Debug)]
pub struct OpenComment {
    pub thread_id: String,
    pub comment: Comment,
}

pub struct ReviewSubmission<'a> {
    pub commit_id: &'a str,
    pub verdict: ReviewVerdict,
    pub body: &'a str,
    pub comments: &'a [Comment],
}

pub struct GhClient {
    crab: Octocrab,
}

impl GhClient {
    pub fn new(crab: Octocrab) -> Self {
        Self { crab }
    }

    pub async fn pr_commits(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> octocrab::Result<Vec<String>> {
        let page = self
            .crab
            .pulls(owner, repo)
            .pr_commits(number)
            .per_page(100)
            .send()
            .await?;
        Ok(page.items.into_iter().map(|c| c.sha).collect())
    }

    /// Opens a single review carrying every inline comment. Multi-line
    /// comments use `start_line`/`line` on the new side of the diff.
    pub async fn submit_review(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        review: ReviewSubmission<'_>,
    ) -> octocrab::Result<u64> {
        let comments: Vec<_> = review
            .comments
            .iter()
            .map(|c| {
                let mut comment = json!({
                    "path": c.file,
                    "body": comment_body(c),
                    "line": c.lines.end,
                    "side": "RIGHT",
                });
                if c.lines.start != c.lines.end {
                    comment["start_line"] = c.lines.start.into();
                    comment["start_side"] = "RIGHT".into();
                }
                comment
            })
            .collect();

        let created: Review = self
            .crab
            .post(
                format!("/repos/{owner}/{repo}/pulls/{number}/reviews"),
                Some(&json!({
                    "commit_id": review.commit_id,
                    "body": review.body,
                    "event": review.verdict.as_str(),
                    "comments": comments,
                })),
            )
            .await?;
        Ok(*created.id)
    }

    /// The PR's unresolved Guardian review threads (first comment carries
    /// the marker or a legacy severity badge), ready to replay in the next
    /// review prompt and to resolve by thread id.
    pub async fn open_guardian_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<OpenComment>, Error> {
        let query = json!({
            "query": "query($owner:String!,$name:String!,$number:Int!){\
                repository(owner:$owner,name:$name){\
                    pullRequest(number:$number){\
                        reviewThreads(first:100){\
                            nodes{id isResolved comments(first:1){\
                                nodes{body path line startLine originalLine originalStartLine}\
                            }}\
                        }\
                    }\
                }\
            }",
            "variables": { "owner": owner, "name": repo, "number": number },
        });
        let data: serde_json::Value = self.crab.graphql(&query).await?;
        check_graphql_errors(&data)?;
        Ok(parse_open_threads(&data))
    }

    /// Resolves the given review threads (GraphQL node ids). Thread
    /// resolution only exists in the GraphQL API. Returns how many threads
    /// were resolved before the first failure.
    pub async fn resolve_threads(&self, thread_ids: &[String]) -> Result<usize, Error> {
        for (resolved, thread_id) in thread_ids.iter().enumerate() {
            let mutation = json!({
                "query": "mutation($threadId:ID!){\
                    resolveReviewThread(input:{threadId:$threadId}){thread{id}}\
                }",
                "variables": { "threadId": thread_id },
            });
            let outcome: Result<serde_json::Value, Error> = self
                .crab
                .graphql(&mutation)
                .await
                .map_err(Error::from)
                .and_then(|data| check_graphql_errors(&data).map(|()| data));
            if let Err(e) = outcome {
                return Err(format!("resolved {resolved}/{} threads: {e}", thread_ids.len()).into());
            }
        }
        Ok(thread_ids.len())
    }

    pub async fn merge(&self, owner: &str, repo: &str, number: u64) -> octocrab::Result<()> {
        self.crab.pulls(owner, repo).merge(number).send().await?;
        Ok(())
    }
}

/// GraphQL failures arrive as HTTP 200 with an `errors` array that octocrab
/// does not turn into `Err`; without this check a denied mutation counts as
/// success.
fn check_graphql_errors(data: &serde_json::Value) -> Result<(), Error> {
    match data.get("errors").and_then(|e| e.as_array()) {
        Some(errors) if !errors.is_empty() => {
            Err(format!("GraphQL errors: {}", serde_json::Value::Array(errors.clone())).into())
        }
        _ => Ok(()),
    }
}

/// Extracts the unresolved Guardian threads from the reviewThreads query
/// response. Threads whose first comment is not ours (no marker, no legacy
/// badge) are someone else's conversation and are skipped.
fn parse_open_threads(data: &serde_json::Value) -> Vec<OpenComment> {
    let threads = data
        .pointer("/data/repository/pullRequest/reviewThreads/nodes")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();

    threads
        .iter()
        .filter(|t| !t["isResolved"].as_bool().unwrap_or(true))
        .filter_map(|t| {
            let first = t.pointer("/comments/nodes/0")?;
            let body = first["body"].as_str()?;
            let parsed = parse_comment_body(body);
            if parsed.is_none() && body.starts_with(MARKER) {
                tracing::warn!(body, "unparseable Guardian comment; thread skipped");
            }
            let (severity, text) = parsed?;
            // outdated comments lose `line`; fall back to the original anchor
            let end = first["line"]
                .as_u64()
                .or_else(|| first["originalLine"].as_u64())? as usize;
            let start = first["startLine"]
                .as_u64()
                .or_else(|| first["originalStartLine"].as_u64())
                .map_or(end, |l| l as usize);
            Some(OpenComment {
                thread_id: t["id"].as_str()?.to_owned(),
                comment: Comment {
                    severity,
                    text,
                    file: first["path"].as_str()?.to_owned(),
                    lines: LineRange { start, end },
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(severity: Severity, text: &str) -> Comment {
        Comment {
            severity,
            text: text.into(),
            file: "src/a.rs".into(),
            lines: LineRange { start: 1, end: 2 },
        }
    }

    #[test]
    fn comment_body_leads_with_the_marker_and_badge() {
        assert_eq!(
            comment_body(&comment(Severity::Bug, "finding")),
            "[Repo Guardian] \u{1f534} **Bug**\n\nfinding"
        );
    }

    #[test]
    fn comment_body_round_trips_through_parse() {
        for severity in [Severity::Bug, Severity::Design, Severity::Nit] {
            let c = comment(severity, "some\n\nmultiline finding");
            let (parsed_severity, text) = parse_comment_body(&comment_body(&c)).unwrap();
            assert_eq!(parsed_severity, severity);
            assert_eq!(text, c.text);
        }
    }

    #[test]
    fn legacy_bodies_without_the_marker_parse_by_badge() {
        // comments posted before the marker existed start with the badge
        let (severity, text) =
            parse_comment_body("\u{1f7e2} **Nit**\n\nolder finding").unwrap();
        assert_eq!(severity, Severity::Nit);
        assert_eq!(text, "older finding");
    }

    #[test]
    fn human_comments_do_not_parse() {
        assert!(parse_comment_body("I think this is fine as is").is_none());
        assert!(parse_comment_body("[Repo Guardian] but no badge").is_none());
    }

    fn thread(id: &str, resolved: bool, first_comment: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "isResolved": resolved,
            "comments": { "nodes": [first_comment] }
        })
    }

    fn threads_response(threads: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "data": { "repository": { "pullRequest": { "reviewThreads": { "nodes": threads } } } }
        })
    }

    #[test]
    fn parse_open_threads_keeps_unresolved_guardian_threads_only() {
        let data = threads_response(vec![
            thread(
                "T_marker",
                false,
                serde_json::json!({
                    "body": comment_body(&comment(Severity::Bug, "overflow")),
                    "path": "src/a.rs",
                    "line": 9, "startLine": 4,
                    "originalLine": 9, "originalStartLine": 4
                }),
            ),
            thread(
                "T_resolved",
                true,
                serde_json::json!({
                    "body": comment_body(&comment(Severity::Nit, "done")),
                    "path": "src/b.rs",
                    "line": 1, "startLine": null,
                    "originalLine": 1, "originalStartLine": null
                }),
            ),
            thread(
                "T_human",
                false,
                serde_json::json!({
                    "body": "why this way?",
                    "path": "src/c.rs",
                    "line": 3, "startLine": null,
                    "originalLine": 3, "originalStartLine": null
                }),
            ),
        ]);

        let open = parse_open_threads(&data);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].thread_id, "T_marker");
        assert_eq!(open[0].comment.severity, Severity::Bug);
        assert_eq!(open[0].comment.text, "overflow");
        assert_eq!(open[0].comment.file, "src/a.rs");
        assert_eq!(open[0].comment.lines, LineRange { start: 4, end: 9 });
    }

    #[test]
    fn parse_open_threads_accepts_legacy_badge_comments() {
        let data = threads_response(vec![thread(
            "T_legacy",
            false,
            serde_json::json!({
                "body": "\u{1f7e2} **Nit**\n\nolder finding",
                "path": "frontend/index.html",
                "line": 24, "startLine": 23,
                "originalLine": 24, "originalStartLine": 23
            }),
        )]);

        let open = parse_open_threads(&data);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].comment.severity, Severity::Nit);
        assert_eq!(open[0].comment.lines, LineRange { start: 23, end: 24 });
    }

    #[test]
    fn parse_open_threads_falls_back_to_original_lines_when_outdated() {
        // an outdated comment loses its current-diff anchor (`line` is null)
        let data = threads_response(vec![thread(
            "T_outdated",
            false,
            serde_json::json!({
                "body": comment_body(&comment(Severity::Design, "shape")),
                "path": "src/a.rs",
                "line": null, "startLine": null,
                "originalLine": 7, "originalStartLine": null
            }),
        )]);

        let open = parse_open_threads(&data);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].comment.lines, LineRange { start: 7, end: 7 });
    }

    #[test]
    fn graphql_errors_are_surfaced_not_swallowed() {
        // GraphQL failures come back HTTP 200 with an `errors` array
        let denied = serde_json::json!({
            "data": { "resolveReviewThread": null },
            "errors": [{ "message": "Resource not accessible by personal access token" }]
        });
        let err = check_graphql_errors(&denied).unwrap_err();
        assert!(err.to_string().contains("Resource not accessible"));

        assert!(check_graphql_errors(&serde_json::json!({ "data": {} })).is_ok());
        assert!(check_graphql_errors(&serde_json::json!({ "data": {}, "errors": [] })).is_ok());
    }
}
