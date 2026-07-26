//! Orchestrates one review round: resolve the repo, ask Guardian, post the
//! review, resolve fixed threads, and merge when allowed.

use std::sync::Arc;

use crate::{
    App,
    github::{OpenComment, ReviewSubmission, ReviewVerdict, payload::PullRequestWH},
    guardian::{Comment, ReviewResult, Severity},
    repos,
    state::BeginReview,
};

use tracing::{error, info, warn};

type Error = Box<dyn std::error::Error + Send + Sync>;

pub async fn process(app: Arc<App>, event: PullRequestWH) {
    let (repository, pull_request, number, synchronize) = match event {
        PullRequestWH::Opened(e) => (e.repository, e.pull_request, e.number, false),
        PullRequestWH::Synchronize(e) => (e.repository, e.pull_request, e.number, true),
        PullRequestWH::Unsupported => return,
    };

    let key = format!("{}#{number}", repository.full_name);
    let head_sha = pull_request.head.sha.clone();

    match app.store.begin_review(&key, &head_sha).await {
        BeginReview::Proceed => {}
        skip => {
            info!(%key, sha = %head_sha, reason = ?skip, "review skipped");
            return;
        }
    }

    info!(%key, sha = %head_sha, "review started");
    let started = std::time::Instant::now();
    let author = pull_request.user.login.as_deref();
    match review_round(
        &app,
        &repository,
        author,
        number,
        &key,
        &head_sha,
        synchronize,
    )
    .await
    {
        Ok(()) => info!(
            %key,
            sha = %head_sha,
            elapsed_s = started.elapsed().as_secs_f32(),
            "review completed"
        ),
        Err(e) => {
            app.store.mark_failed(&key).await;
            error!(
                %key,
                sha = %head_sha,
                elapsed_s = started.elapsed().as_secs_f32(),
                error = %error_chain(e.as_ref()),
                "review failed"
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn review_round(
    app: &App,
    repository: &crate::github::payload::Repository,
    author: Option<&str>,
    number: i32,
    key: &str,
    head_sha: &str,
    synchronize: bool,
) -> Result<(), Error> {
    let (owner, name) = repository
        .owner_and_name()
        .ok_or_else(|| format!("malformed repository full_name: {}", repository.full_name))?;
    let pr = number as u64;

    let checkout = repos::resolve(&app.config.repos_path, repository).await?;
    repos::fetch_pr(&checkout, number).await?;
    let worktree = repos::pr_worktree(&checkout, number).await?;

    let commits = app.gh.pr_commits(owner, name, pr).await?;
    // GitHub's unresolved review threads are the source of truth for what
    // Guardian flagged earlier; local state doesn't track comments. A just
    // opened PR can't have Guardian threads yet, so only pushes fetch them.
    let previous = if synchronize {
        app.gh.open_guardian_comments(owner, name, pr).await?
    } else {
        vec![]
    };

    let result = app
        .guardian
        .review(&commits, &worktree.to_string_lossy(), &previous)
        .await;
    // the worktree is only for Guardian; a leftover one is recreated next round
    if let Err(e) = repos::remove_pr_worktree(&checkout, number).await {
        warn!(%key, error = %e, "failed to remove worktree");
    }
    let result = result?;

    // Guardian said which earlier comments the new code fixes; resolve their
    // threads on GitHub.
    let fixed_threads = fixed_thread_ids(&result.resolved_previous, &previous);
    let mut resolved = 0;
    if !fixed_threads.is_empty() {
        resolved = app.gh.resolve_threads(&fixed_threads).await?;
        info!(%key, resolved, "resolved fixed comment threads");
    }

    let still_open = unresolved_previous(&result.unresolved_previous, &previous);
    let approved = decide_approval(&result, &still_open);
    // GitHub rejects APPROVE/REQUEST_CHANGES reviews on one's own PR, so
    // when the author is the account Guardian runs as, the verdict is a
    // neutral comment and the body carries the outcome.
    let own_pr = app
        .username
        .as_deref()
        .is_some_and(|user| author == Some(user));
    let verdict = choose_verdict(approved, own_pr);
    let body = review_body(
        approved,
        own_pr,
        &result.comments,
        resolved,
        still_open.len(),
    );
    let submission = ReviewSubmission {
        commit_id: head_sha,
        verdict,
        body: &body,
        comments: &result.comments,
    };
    let review_id = match app.gh.submit_review(owner, name, pr, submission).await {
        Ok(id) => id,
        // GitHub rejects the whole review when any inline anchor falls
        // outside the diff; the findings still must land, so retry with
        // them folded into the review body.
        Err(e) => {
            warn!(
                %key,
                error = %error_chain(&e),
                "inline review rejected; retrying without inline comments"
            );
            app.gh
                .submit_review(
                    owner,
                    name,
                    pr,
                    ReviewSubmission {
                        commit_id: head_sha,
                        verdict,
                        body: &fallback_body(&body, &result.comments),
                        comments: &[],
                    },
                )
                .await?
        }
    };
    info!(
        %key,
        review_id,
        approved,
        own_pr,
        comments = result.comments.len(),
        "review posted"
    );

    app.store.finish_review(key).await;

    if approved && app.config.auto_merge {
        // A failed merge (branch protection, conflicts) shouldn't mark the
        // review round as failed -- the review itself already landed.
        match app.gh.merge(owner, name, pr).await {
            Ok(()) => info!(%key, "auto-merged"),
            Err(e) => warn!(%key, error = %e, "auto-merge failed"),
        }
    }

    Ok(())
}

/// Full error chain, since some `Display` impls (octocrab's GitHub variant)
/// print only the outermost layer and bury the API message in `source()`.
fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

/// Review body carrying the inline comments as plain text, for when GitHub
/// rejects the anchored submission.
fn fallback_body(body: &str, comments: &[Comment]) -> String {
    use std::fmt::Write as _;

    let mut out = format!(
        "{body}\n---\n_GitHub rejected the inline anchors; findings listed here instead._\n"
    );
    for c in comments {
        let _ = write!(
            out,
            "\n**`{}:{}`** {}\n\n{}\n",
            c.file,
            c.lines,
            c.severity.badge(),
            c.text
        );
    }
    out
}

/// Thread ids of the previously-open comments the model marked fixed.
/// `resolved_previous` comes straight from the model: out-of-range indices
/// are dropped and repeats deduplicated so the resolved count stays honest.
fn fixed_thread_ids(resolved_previous: &[usize], previous: &[OpenComment]) -> Vec<String> {
    let mut ids: Vec<String> = resolved_previous
        .iter()
        .filter_map(|&i| previous.get(i))
        .map(|p| p.thread_id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// The previously-open comments the model reported as still unresolved.
/// `unresolved_previous` comes straight from the model, so -- like
/// [`fixed_thread_ids`] -- out-of-range indices are dropped and repeats
/// deduplicated, keeping the reported count honest.
fn unresolved_previous<'a>(
    unresolved_previous: &[usize],
    previous: &'a [OpenComment],
) -> Vec<&'a OpenComment> {
    let mut indices: Vec<usize> = unresolved_previous
        .iter()
        .copied()
        .filter(|&i| i < previous.len())
        .collect();
    indices.sort_unstable();
    indices.dedup();
    indices.iter().map(|&i| &previous[i]).collect()
}

/// Guardian is asked to approve only when nothing above nit severity is
/// found; enforce that here too so an inconsistent result can never approve
/// (or auto-merge) a PR with an open bug or design finding. An earlier
/// finding the push did not fix still counts: it is above nit and open, so
/// it blocks just like a fresh one, even though the model was told not to
/// re-report it as a new comment.
fn decide_approval(result: &ReviewResult, still_open: &[&OpenComment]) -> bool {
    result.approved
        && result.comments.iter().all(|c| c.severity == Severity::Nit)
        && still_open
            .iter()
            .all(|p| p.comment.severity == Severity::Nit)
}

fn choose_verdict(approved: bool, own_pr: bool) -> ReviewVerdict {
    if own_pr {
        ReviewVerdict::Comment
    } else if approved {
        ReviewVerdict::Approve
    } else {
        ReviewVerdict::RequestChanges
    }
}

fn review_body(
    approved: bool,
    own_pr: bool,
    comments: &[Comment],
    resolved: usize,
    unresolved: usize,
) -> String {
    use std::fmt::Write as _;

    let verdict = match (approved, own_pr) {
        (true, true) => "\u{2705} All good to merge",
        (true, false) => "\u{2705} Approved",
        (false, _) => "\u{26a0}\u{fe0f} Changes requested",
    };
    let mut body = format!("## \u{1f6e1}\u{fe0f} Guardian review\n\n**Verdict: {verdict}**\n");

    if comments.is_empty() {
        // If there are no unresolved findigns we can celebrate.
        if unresolved == 0 {
            body.push_str("\n\u{1f389} No findings.\n");
        } else {
            // Otherwise, be aware
            let _ = write!(
                body,
                "\n\u{1f4ac} No new findings. But there are {unresolved} earlier comment(s) unresolved yet.\n",
            );
        }
    } else {
        body.push_str("\n| Severity | Count |\n| --- | --- |\n");
        for severity in [Severity::Bug, Severity::Design, Severity::Nit] {
            let count = comments.iter().filter(|c| c.severity == severity).count();
            if count > 0 {
                let _ = writeln!(body, "| {} | {count} |", severity.badge());
            }
        }
        body.push_str("\nSee the inline comments for details.\n");
    }

    if resolved > 0 {
        let _ = write!(
            body,
            "\n\u{267b}\u{fe0f} Resolved {resolved} earlier comment(s) fixed by this push.\n"
        );
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::LineRange;

    fn comment(severity: Severity) -> Comment {
        Comment {
            severity,
            text: "finding".into(),
            file: "src/a.rs".into(),
            lines: LineRange { start: 1, end: 1 },
        }
    }

    fn open(thread_id: &str, severity: Severity) -> OpenComment {
        OpenComment {
            thread_id: thread_id.into(),
            comment: comment(severity),
        }
    }

    #[test]
    fn fixed_thread_ids_drop_out_of_range_and_repeated_indices() {
        let previous: Vec<OpenComment> = ["T_a", "T_b"]
            .into_iter()
            .map(|id| open(id, Severity::Nit))
            .collect();

        // the model repeated index 1 and invented index 9
        let ids = fixed_thread_ids(&[1, 9, 1, 0], &previous);
        assert_eq!(ids, vec!["T_a".to_owned(), "T_b".to_owned()]);
    }

    #[test]
    fn approval_is_vetoed_by_findings_above_nit() {
        let result = ReviewResult {
            approved: true,
            comments: vec![comment(Severity::Bug), comment(Severity::Nit)],
            resolved_previous: vec![],
            unresolved_previous: vec![],
        };
        assert!(!decide_approval(&result, &[]));

        // design findings ask for a fix too; only nits may pass
        let result = ReviewResult {
            approved: true,
            comments: vec![comment(Severity::Design), comment(Severity::Nit)],
            resolved_previous: vec![],
            unresolved_previous: vec![],
        };
        assert!(!decide_approval(&result, &[]));

        let result = ReviewResult {
            approved: true,
            comments: vec![comment(Severity::Nit)],
            resolved_previous: vec![],
            unresolved_previous: vec![],
        };
        assert!(decide_approval(&result, &[]));

        let result = ReviewResult {
            approved: false,
            comments: vec![],
            resolved_previous: vec![],
            unresolved_previous: vec![],
        };
        assert!(!decide_approval(&result, &[]));
    }

    #[test]
    fn approval_is_vetoed_by_previous_findings_left_unresolved() {
        // the push fixed nothing new, and the model reported no fresh
        // findings because it was told not to re-report still-open ones
        let result = ReviewResult {
            approved: true,
            comments: vec![],
            resolved_previous: vec![],
            unresolved_previous: vec![0],
        };

        let previous = vec![open("T_a", Severity::Design)];
        let still_open = unresolved_previous(&result.unresolved_previous, &previous);
        assert_eq!(still_open.len(), 1);
        assert!(!decide_approval(&result, &still_open));

        // an unresolved nit is optional polish, so it still may pass
        let previous = vec![open("T_a", Severity::Nit)];
        let still_open = unresolved_previous(&result.unresolved_previous, &previous);
        assert!(decide_approval(&result, &still_open));

        // a previous bug the model listed as both fixed and still open
        // blocks: resolving its thread doesn't make the finding go away
        let result = ReviewResult {
            approved: true,
            comments: vec![],
            resolved_previous: vec![0],
            unresolved_previous: vec![0],
        };
        let previous = vec![open("T_a", Severity::Bug)];
        let still_open = unresolved_previous(&result.unresolved_previous, &previous);
        assert!(!decide_approval(&result, &still_open));
    }

    #[test]
    fn unresolved_previous_drops_out_of_range_and_repeated_indices() {
        let previous = vec![open("T_a", Severity::Bug), open("T_b", Severity::Nit)];

        // the model repeated index 1 and invented index 9
        let still_open = unresolved_previous(&[1, 9, 1, 0], &previous);
        let ids: Vec<&str> = still_open.iter().map(|p| p.thread_id.as_str()).collect();
        assert_eq!(ids, vec!["T_a", "T_b"]);

        assert!(unresolved_previous(&[0], &[]).is_empty());
    }

    #[test]
    fn review_body_summarizes_verdict_and_severities() {
        let body = review_body(true, false, &[], 0, 0);
        assert!(body.contains("## \u{1f6e1}\u{fe0f} Guardian review"));
        assert!(body.contains("**Verdict: \u{2705} Approved**"));
        assert!(body.contains("\u{1f389} No findings."));
        assert!(!body.contains("Resolved"));

        let body = review_body(true, true, &[], 2, 0);
        assert!(body.contains("\u{2705} All good to merge"));
        assert!(body.contains("\u{267b}\u{fe0f} Resolved 2 earlier comment(s)"));

        let body = review_body(true, true, &[], 0, 2);
        assert!(body.contains("\u{2705} All good to merge"));
        assert!(body.contains(
            "\u{1f4ac} No new findings. But there are 2 earlier comment(s) unresolved yet.\n"
        ));

        let body = review_body(
            false,
            false,
            &[comment(Severity::Bug), comment(Severity::Design)],
            0,
            0,
        );
        assert!(body.contains("\u{26a0}\u{fe0f} Changes requested"));
        assert!(body.contains("| \u{1f534} **Bug** | 1 |"));
        assert!(body.contains("| \u{1f7e1} **Design** | 1 |"));
        // zero-count severities are omitted from the table
        assert!(!body.contains("**Nit**"));
    }

    #[test]
    fn error_chain_prints_buried_sources() {
        #[derive(Debug)]
        struct Opaque(std::io::Error);
        impl std::fmt::Display for Opaque {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "GitHub")
            }
        }
        impl std::error::Error for Opaque {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let e = Opaque(std::io::Error::other("line must be part of the diff"));
        assert_eq!(error_chain(&e), "GitHub: line must be part of the diff");
    }

    #[test]
    fn fallback_body_folds_findings_into_the_review_body() {
        let body = review_body(false, false, &[comment(Severity::Bug)], 0, 0);
        let out = fallback_body(&body, &[comment(Severity::Bug)]);

        assert!(out.contains("Changes requested"));
        assert!(out.contains("inline anchors"));
        assert!(out.contains("**`src/a.rs:1`** \u{1f534} **Bug**"));
        assert!(out.contains("\nfinding\n"));
    }

    #[test]
    fn own_prs_always_get_a_neutral_comment_review() {
        assert_eq!(choose_verdict(true, true), ReviewVerdict::Comment);
        assert_eq!(choose_verdict(false, true), ReviewVerdict::Comment);
        assert_eq!(choose_verdict(true, false), ReviewVerdict::Approve);
        assert_eq!(choose_verdict(false, false), ReviewVerdict::RequestChanges);
    }
}
