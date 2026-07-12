//! Local working copies of the repositories under review.

use std::{
    fmt::{self, Display},
    path::{Path, PathBuf},
};

use tokio::process::Command;

use crate::github::payload::Repository;

#[derive(Debug)]
pub enum RepoError {
    Io(std::io::Error),
    Git { args: Vec<String>, stderr: String },
}

impl Display for RepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepoError::Io(e) => write!(f, "repo io error: {e}"),
            RepoError::Git { args, stderr } => {
                write!(f, "git {} failed: {}", args.join(" "), stderr.trim())
            }
        }
    }
}

impl std::error::Error for RepoError {}

impl From<std::io::Error> for RepoError {
    fn from(e: std::io::Error) -> Self {
        RepoError::Io(e)
    }
}

/// Returns the local checkout for `repo`, cloning it under
/// `<repos_path>/<owner>/<name>` on first sight and fetching otherwise.
pub async fn resolve(repos_path: &Path, repo: &Repository) -> Result<PathBuf, RepoError> {
    let dest = repos_path.join(&repo.full_name);
    if dest.join(".git").exists() {
        git(&dest, &["fetch", "origin"]).await?;
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        tracing::info!(repo = %repo.full_name, dest = %dest.display(), "cloning");
        let dest_str = dest.to_string_lossy();
        git(repos_path, &["clone", &repo.clone_url, &dest_str]).await?;
    }
    Ok(dest)
}

/// Fetches the PR's head so its commits are inspectable locally. GitHub
/// exposes every PR at `refs/pull/<n>/head`.
pub async fn fetch_pr(checkout: &Path, number: i32) -> Result<(), RepoError> {
    let refspec = format!("+refs/pull/{number}/head:refs/guardian/pr/{number}");
    git(checkout, &["fetch", "origin", &refspec]).await
}

fn worktree_dir(checkout: &Path, number: i32) -> PathBuf {
    let name = checkout
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    checkout.with_file_name(format!("{name}-pr-{number}"))
}

/// Materializes the PR head as a detached worktree next to the shared
/// checkout (`<name>-pr-<n>`), so the reviewer reads the PR's version of
/// the files and concurrent reviews never fight over one working tree.
/// Call after [`fetch_pr`]; on a new push the existing worktree is moved
/// to the new head.
pub async fn pr_worktree(checkout: &Path, number: i32) -> Result<PathBuf, RepoError> {
    let path = worktree_dir(checkout, number);
    let refname = format!("refs/guardian/pr/{number}");
    if path.exists() {
        git(&path, &["checkout", "--detach", &refname]).await?;
    } else {
        // drop stale registrations (e.g. a manually deleted worktree)
        git(checkout, &["worktree", "prune"]).await?;
        let path_str = path.to_string_lossy();
        git(
            checkout,
            &["worktree", "add", "--detach", &path_str, &refname],
        )
        .await?;
    }
    Ok(path)
}

pub async fn remove_pr_worktree(checkout: &Path, number: i32) -> Result<(), RepoError> {
    let path = worktree_dir(checkout, number);
    if !path.exists() {
        return Ok(());
    }
    let path_str = path.to_string_lossy();
    git(checkout, &["worktree", "remove", "--force", &path_str]).await
}

/// Inline credential helper that hands `$GITHUB_TOKEN` to git at runtime.
/// The token stays in the environment: it never appears in argv, in the
/// checkout's .git/config, or in logged [`RepoError::Git`] args (the clone
/// URL stays credential-free).
const CREDENTIAL_HELPER: &str =
    r#"!f() { echo "username=x-access-token"; echo "password=${GITHUB_TOKEN}"; }; f"#;

async fn git(cwd: &Path, args: &[&str]) -> Result<(), RepoError> {
    tracing::debug!(cwd = %cwd.display(), ?args, "git");
    let mut cmd = Command::new("git");
    if std::env::var("GITHUB_TOKEN").is_ok_and(|t| !t.is_empty()) {
        cmd.args(["-c", &format!("credential.helper={CREDENTIAL_HELPER}")]);
    }
    let output = cmd.args(args).current_dir(cwd).output().await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(RepoError::Git {
            args: args.iter().map(|s| s.to_string()).collect(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::payload::User;

    fn run(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn upstream(dir: &Path) -> Repository {
        let origin = dir.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        run(&origin, &["init", "-b", "main"]);
        run(&origin, &["config", "user.email", "t@t"]);
        run(&origin, &["config", "user.name", "t"]);
        std::fs::write(origin.join("a.txt"), "hello\n").unwrap();
        run(&origin, &["add", "."]);
        run(&origin, &["commit", "-m", "init"]);
        // a PR branch whose content differs from main, exposed the way
        // GitHub exposes PRs
        run(&origin, &["checkout", "-b", "pr"]);
        std::fs::write(origin.join("a.txt"), "pr version\n").unwrap();
        run(&origin, &["commit", "-am", "pr change"]);
        run(&origin, &["update-ref", "refs/pull/7/head", "HEAD"]);
        run(&origin, &["checkout", "main"]);

        Repository {
            id: 1,
            clone_url: origin.to_string_lossy().into_owned(),
            full_name: "octocat/repo".into(),
            name: "repo".into(),
            owner: User {
                id: 1,
                avatar_url: String::new(),
                email: None,
                login: Some("octocat".into()),
                name: None,
                url: String::new(),
            },
            private: false,
        }
    }

    #[tokio::test]
    async fn resolve_clones_then_fetches() {
        let dir = tempfile::tempdir().unwrap();
        let repos_path = dir.path().join("repos");
        std::fs::create_dir_all(&repos_path).unwrap();
        let repo = upstream(dir.path());

        // first sight: clone
        let checkout = resolve(&repos_path, &repo).await.unwrap();
        assert_eq!(checkout, repos_path.join("octocat/repo"));
        assert!(checkout.join("a.txt").exists());

        // second sight: fetch on the existing checkout
        let again = resolve(&repos_path, &repo).await.unwrap();
        assert_eq!(again, checkout);
    }

    #[tokio::test]
    async fn fetch_pr_makes_the_pr_head_available() {
        let dir = tempfile::tempdir().unwrap();
        let repos_path = dir.path().join("repos");
        std::fs::create_dir_all(&repos_path).unwrap();
        let repo = upstream(dir.path());

        let checkout = resolve(&repos_path, &repo).await.unwrap();
        fetch_pr(&checkout, 7).await.unwrap();

        let out = std::process::Command::new("git")
            .args(["rev-parse", "refs/guardian/pr/7"])
            .current_dir(&checkout)
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    #[tokio::test]
    async fn pr_worktree_checks_out_the_pr_head() {
        let dir = tempfile::tempdir().unwrap();
        let repos_path = dir.path().join("repos");
        std::fs::create_dir_all(&repos_path).unwrap();
        let repo = upstream(dir.path());

        let checkout = resolve(&repos_path, &repo).await.unwrap();
        fetch_pr(&checkout, 7).await.unwrap();
        let worktree = pr_worktree(&checkout, 7).await.unwrap();

        // the worktree holds the PR's version, the shared checkout is untouched
        let read = |p: &Path| std::fs::read_to_string(p.join("a.txt")).unwrap();
        assert_eq!(read(&worktree), "pr version\n");
        assert_eq!(read(&checkout), "hello\n");

        // a new push moves the existing worktree to the new head
        let origin = dir.path().join("origin");
        run(&origin, &["checkout", "pr"]);
        std::fs::write(origin.join("a.txt"), "pr v2\n").unwrap();
        run(&origin, &["commit", "-am", "pr push"]);
        run(&origin, &["update-ref", "refs/pull/7/head", "HEAD"]);
        run(&origin, &["checkout", "main"]);

        fetch_pr(&checkout, 7).await.unwrap();
        let again = pr_worktree(&checkout, 7).await.unwrap();
        assert_eq!(again, worktree);
        assert_eq!(read(&worktree), "pr v2\n");

        remove_pr_worktree(&checkout, 7).await.unwrap();
        assert!(!worktree.exists());
        // removing an already-removed worktree is a no-op
        remove_pr_worktree(&checkout, 7).await.unwrap();
    }

    #[test]
    fn credential_helper_feeds_the_token_from_env() {
        use std::io::Write;

        // isolated config so only the inline helper answers
        let home = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("git")
            .args([
                "-c",
                &format!("credential.helper={CREDENTIAL_HELPER}"),
                "credential",
                "fill",
            ])
            .env("GITHUB_TOKEN", "tok-123")
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol=https\nhost=github.com\n\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success());
        assert!(stdout.contains("username=x-access-token"), "{stdout}");
        assert!(stdout.contains("password=tok-123"), "{stdout}");
    }

    #[tokio::test]
    async fn clone_failure_surfaces_git_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository {
            clone_url: dir.path().join("missing").to_string_lossy().into_owned(),
            ..upstream(dir.path())
        };

        let err = resolve(dir.path(), &repo).await.unwrap_err();
        assert!(matches!(err, RepoError::Git { .. }));
    }
}
