//! Thin wrapper over octocrab for the operations the pipeline needs.

use octocrab::{
    Octocrab,
    models::pulls::{Review, ReviewComment},
};
use serde_json::json;

use crate::guardian::Comment;

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

/// Body posted to GitHub for an inline comment: severity badge as a
/// heading line, then the finding.
pub fn comment_body(comment: &Comment) -> String {
    format!("{}\n\n{}", comment.severity.badge(), comment.text)
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

    /// The inline comments a review created, used to learn their database
    /// ids for later thread resolution.
    pub async fn review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        review_id: u64,
    ) -> octocrab::Result<Vec<ReviewComment>> {
        self.crab
            .get(
                format!(
                    "/repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}/comments?per_page=100"
                ),
                None::<&()>,
            )
            .await
    }

    /// Resolves the review threads containing any of `comment_ids`. Thread
    /// resolution only exists in the GraphQL API, so this queries the PR's
    /// threads and fires one mutation per match. Returns how many threads
    /// were resolved.
    pub async fn resolve_comment_threads(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        comment_ids: &[u64],
    ) -> octocrab::Result<usize> {
        let query = json!({
            "query": "query($owner:String!,$name:String!,$number:Int!){\
                repository(owner:$owner,name:$name){\
                    pullRequest(number:$number){\
                        reviewThreads(first:100){\
                            nodes{id isResolved comments(first:50){nodes{databaseId}}}\
                        }\
                    }\
                }\
            }",
            "variables": { "owner": owner, "name": repo, "number": number },
        });
        let data: serde_json::Value = self.crab.graphql(&query).await?;
        let threads = data
            .pointer("/data/repository/pullRequest/reviewThreads/nodes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut resolved = 0;
        for thread in threads {
            if thread["isResolved"].as_bool().unwrap_or(true) {
                continue;
            }
            let matches = thread
                .pointer("/comments/nodes")
                .and_then(|v| v.as_array())
                .is_some_and(|comments| {
                    comments.iter().any(|c| {
                        c["databaseId"]
                            .as_u64()
                            .is_some_and(|id| comment_ids.contains(&id))
                    })
                });
            if matches {
                let mutation = json!({
                    "query": "mutation($threadId:ID!){\
                        resolveReviewThread(input:{threadId:$threadId}){thread{id}}\
                    }",
                    "variables": { "threadId": thread["id"] },
                });
                let _: serde_json::Value = self.crab.graphql(&mutation).await?;
                resolved += 1;
            }
        }
        Ok(resolved)
    }

    pub async fn merge(&self, owner: &str, repo: &str, number: u64) -> octocrab::Result<()> {
        self.crab.pulls(owner, repo).merge(number).send().await?;
        Ok(())
    }
}
