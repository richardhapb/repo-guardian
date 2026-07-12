//! Guardian is the reviewer: it receives the PR commits and the local repo
//! location, inspects the changes with the Claude Code CLI, and returns a
//! structured verdict.

use std::{
    fmt::{self, Display, Write as _},
    time::Duration,
};

use claude_code::{ClaudeConfig, ClaudeError, CommandRunner, DefaultRunner};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::state::PostedComment;

/// Severity taxonomy borrowed from the mr-review flow.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Wrong behavior, regressions, broken invariants. Blocks approval.
    Bug,
    /// Scope creep, API shape, boundary leaks. Worth pushing back, not blocking.
    Design,
    /// Minor polish, optional.
    Nit,
}

impl Severity {
    pub fn badge(self) -> &'static str {
        match self {
            Severity::Bug => "\u{1f534} **Bug**",
            Severity::Design => "\u{1f7e1} **Design**",
            Severity::Nit => "\u{1f7e2} **Nit**",
        }
    }
}

impl Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Severity::Bug => "bug",
            Severity::Design => "design",
            Severity::Nit => "nit",
        };
        write!(f, "{label}")
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq, Debug)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

impl Display for LineRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}:{}", self.start, self.end)
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, PartialEq, Eq, Debug)]
pub struct Comment {
    pub severity: Severity,
    pub text: String,
    /// Path relative to the repo root.
    pub file: String,
    /// Line range in the new version of the file.
    pub lines: LineRange,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct ReviewResult {
    pub approved: bool,
    pub comments: Vec<Comment>,
    /// Indices into the previously-open comments passed in the prompt that
    /// the latest push resolved.
    #[serde(default)]
    pub resolved_previous: Vec<usize>,
}

pub struct Guardian<R: CommandRunner + Clone = DefaultRunner> {
    runner: R,
    schema: String,
}

impl Guardian {
    pub fn new() -> Self {
        Self::with_runner(DefaultRunner::new("claude"))
    }
}

impl Default for Guardian {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: CommandRunner + Clone> Guardian<R> {
    pub fn with_runner(runner: R) -> Self {
        Self {
            runner,
            schema: review_schema(),
        }
    }

    pub async fn review(
        &self,
        commits: &[String],
        repo_location: &str,
        previous: &[PostedComment],
    ) -> Result<ReviewResult, ClaudeError> {
        let config = ClaudeConfig::builder()
            .json_schema(self.schema.clone())
            .add_dir(repo_location)
            // The Normal preset passes `--tools ""` unless overridden,
            // leaving the reviewer with no tools at all: it must be able to
            // run git and read files. allowed_tools then auto-approves them.
            .tools("Read,Grep,Glob,Bash")
            .allowed_tools(["Read", "Grep", "Glob", "Bash(git:*)"])
            .max_turns(64)
            .timeout(Duration::from_secs(900))
            .build();
        // The CLI is run directly instead of through ClaudeClient::ask:
        // since CLI 2.x the schema-conforming value arrives in a separate
        // `structured_output` field that claude-code 0.1.2's ClaudeResponse
        // does not carry.
        let args = config.to_args(&build_prompt(commits, repo_location, previous));
        let output = tokio::time::timeout(
            config.timeout.unwrap_or(Duration::from_secs(900)),
            self.runner.run(&args),
        )
        .await
        .map_err(|_| ClaudeError::Timeout)?
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ClaudeError::CliNotFound,
            _ => ClaudeError::Io(e),
        })?;
        if !output.status.success() {
            return Err(ClaudeError::NonZeroExit {
                code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let response: CliResponse = serde_json::from_str(extract_json(&stdout))
            .map_err(|e| ClaudeError::StructuredOutputError {
                raw_result: stdout.clone().into_owned(),
                source: e,
            })?;
        tracing::info!(
            duration_ms = response.duration_ms,
            turns = response.num_turns,
            cost_usd = response.total_cost_usd,
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            "claude run finished"
        );
        response.into_review()
    }
}

/// The slice of the CLI's `--output-format json` response we consume.
#[derive(Deserialize)]
struct CliResponse {
    /// Plain-text answer. CLIs older than 2.x serialize the structured
    /// value here.
    #[serde(default)]
    result: String,
    /// Schema-conforming value (CLI 2.x and later).
    structured_output: Option<serde_json::Value>,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    num_turns: u32,
    #[serde(default)]
    total_cost_usd: f64,
    #[serde(default)]
    usage: CliUsage,
}

#[derive(Deserialize, Default)]
struct CliUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

impl CliResponse {
    fn into_review(self) -> Result<ReviewResult, ClaudeError> {
        match self.structured_output {
            Some(value) => serde_json::from_value(value.clone()).map_err(|e| {
                ClaudeError::StructuredOutputError {
                    raw_result: value.to_string(),
                    source: e,
                }
            }),
            None => serde_json::from_str(&self.result).map_err(|e| {
                ClaudeError::StructuredOutputError {
                    raw_result: self.result,
                    source: e,
                }
            }),
        }
    }
}

/// The CLI may wrap the JSON in terminal escape sequences; keep the first
/// `{` through the last `}`.
fn extract_json(stdout: &str) -> &str {
    match (stdout.find('{'), stdout.rfind('}')) {
        (Some(start), Some(end)) if start <= end => &stdout[start..=end],
        _ => stdout,
    }
}

/// The claude CLI validates `--json-schema` with Ajv, which doesn't know
/// draft 2020-12 (schemars' default), so the schema is generated as draft-07.
fn review_schema() -> String {
    let schema = schemars::generate::SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<ReviewResult>();
    serde_json::to_string(&schema).expect("serialize ReviewResult schema")
}

fn build_prompt(commits: &[String], repo_location: &str, previous: &[PostedComment]) -> String {
    let mut prompt = format!(
        "You are Repo Guardian's automated PR reviewer.\n\n\
         Repository checkout: {repo_location}\n\
         Its working tree is checked out at the PR's head commit, so reading \
         files shows the PR's version of the code.\n\
         Commits under review (already fetched locally, oldest first):\n"
    );
    for sha in commits {
        let _ = writeln!(prompt, "- {sha}");
    }
    prompt.push_str(
        "\nInspect the changes with git, prefixing every command with the checkout path, \
         e.g. `git -C <checkout> show <sha>` and `git -C <checkout> diff <first>^..<last>`. \
         Read full files (not just hunks) when correctness depends on surrounding context.\n\n\
         Review for intent vs behavior: does the code do what the commits claim? \
         Verify before flagging -- open the function and check what it returns, grep callers \
         of changed shared code, look at what was deleted. Flag scope creep. \
         Only report real, actionable findings; do not pad the review.\n\n\
         Assign each comment a severity:\n\
         - bug: wrong behavior, regressions, broken invariants (blocks approval)\n\
         - design: scope creep, API shape, boundary leaks (push back, not blocking)\n\
         - nit: minor polish (optional)\n",
    );
    if !previous.is_empty() {
        prompt.push_str(
            "\nThese review comments from earlier rounds are still open:\n",
        );
        for (i, posted) in previous.iter().enumerate() {
            let c = &posted.comment;
            let _ = writeln!(
                prompt,
                "{i}. [{}] {} ({}): {}",
                c.severity, c.file, c.lines, c.text
            );
        }
        prompt.push_str(
            "If the latest code resolves one of them, put its index in `resolved_previous`. \
             Do not re-report still-open ones as new comments.\n",
        );
    }
    prompt.push_str(
        "\nReturn the structured result: `approved` must be true only when there are no \
         bug-severity findings; `comments` entries use file paths relative to the repo root \
         and line ranges in the new version of the file.\n",
    );
    prompt
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::process::ExitStatusExt,
        process::{ExitStatus, Output},
        sync::{Arc, Mutex},
    };

    use super::*;

    #[derive(Clone)]
    struct FakeRunner {
        stdout: String,
        seen_args: Arc<Mutex<Vec<String>>>,
    }

    impl FakeRunner {
        fn returning(result: serde_json::Value) -> Self {
            let response = serde_json::json!({
                "result": result.to_string(),
                "is_error": false,
                "duration_ms": 1,
                "num_turns": 1,
                "session_id": "s",
                "total_cost_usd": 0.0,
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 1,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0
                }
            });
            Self {
                stdout: response.to_string(),
                seen_args: Arc::new(Mutex::new(vec![])),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        async fn run(&self, args: &[String]) -> std::io::Result<Output> {
            *self.seen_args.lock().unwrap() = args.to_vec();
            Ok(Output {
                status: ExitStatus::from_raw(0),
                stdout: self.stdout.clone().into_bytes(),
                stderr: vec![],
            })
        }
    }

    fn previous_comment() -> PostedComment {
        PostedComment {
            comment_id: Some(11),
            comment: Comment {
                severity: Severity::Bug,
                text: "off-by-one in pagination".into(),
                file: "src/page.rs".into(),
                lines: LineRange { start: 4, end: 9 },
            },
        }
    }

    #[tokio::test]
    async fn review_prefers_the_structured_output_field() {
        // CLI 2.x shape: prose in `result`, the value in `structured_output`
        let response = serde_json::json!({
            "type": "result",
            "result": "The PR looks good, approving.",
            "is_error": false,
            "duration_ms": 1,
            "num_turns": 2,
            "session_id": "s",
            "total_cost_usd": 0.0,
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1},
            "structured_output": {"approved": true, "comments": []}
        });
        let runner = FakeRunner {
            stdout: response.to_string(),
            seen_args: Arc::new(Mutex::new(vec![])),
        };
        let guardian = Guardian::with_runner(runner);

        let result = guardian
            .review(&["abc123".into()], "/repos/octocat/repo", &[])
            .await
            .unwrap();

        assert!(result.approved);
        assert!(result.comments.is_empty());
    }

    #[tokio::test]
    async fn review_parses_structured_result() {
        let runner = FakeRunner::returning(serde_json::json!({
            "approved": false,
            "comments": [{
                "severity": "bug",
                "text": "overflow",
                "file": "src/a.rs",
                "lines": {"start": 1, "end": 3}
            }],
            "resolved_previous": [0]
        }));
        let guardian = Guardian::with_runner(runner.clone());

        let result = guardian
            .review(
                &["abc123".into(), "def456".into()],
                "/repos/octocat/repo",
                &[previous_comment()],
            )
            .await
            .unwrap();

        assert!(!result.approved);
        assert_eq!(result.comments.len(), 1);
        assert_eq!(result.comments[0].severity, Severity::Bug);
        assert_eq!(result.resolved_previous, vec![0]);

        let args = runner.seen_args.lock().unwrap().join("\n");
        assert!(args.contains("/repos/octocat/repo"));
        assert!(args.contains("abc123"));
        assert!(args.contains("off-by-one in pagination"));
        assert!(args.contains("resolved_previous"));
        // the preset's `--tools ""` must be overridden or the reviewer
        // has no tools at all
        assert!(args.contains("--tools\nRead,Grep,Glob,Bash"));
    }

    #[test]
    fn schema_is_draft07_for_the_claude_cli() {
        let schema: serde_json::Value = serde_json::from_str(&review_schema()).unwrap();
        assert_eq!(
            schema["$schema"],
            "http://json-schema.org/draft-07/schema#"
        );
        let props = schema.get("properties").expect("schema has properties");
        for field in ["approved", "comments", "resolved_previous"] {
            assert!(props.get(field).is_some(), "missing {field}");
        }
    }

    #[test]
    fn prompt_omits_previous_section_when_empty() {
        let prompt = build_prompt(&["abc".into()], "/repos/r", &[]);
        assert!(!prompt.contains("earlier rounds"));
        assert!(prompt.contains("- abc"));
    }

    #[test]
    fn line_range_displays_single_line_and_ranges() {
        assert_eq!(LineRange { start: 3, end: 3 }.to_string(), "3");
        assert_eq!(LineRange { start: 4, end: 9 }.to_string(), "4:9");
    }
}
