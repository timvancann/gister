use anyhow::Context;
use chrono::{TimeDelta, prelude::*};
use git2::Repository;
use itertools::Itertools;
use ollama_rs::Ollama;
use ollama_rs::generation::completion::GenerationResponse;
use ollama_rs::generation::completion::request::GenerationRequest;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::process::Stdio;
use std::{
    fmt::Debug,
    path::{Path, PathBuf},
};
use url::Url;

use termimad::crossterm::style::Stylize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use clap::Parser;
use serde_json;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    dir: Option<PathBuf>,
    #[arg(
        long,
        short,
        help = "The git user to check, default is the system git user (git config user.name)"
    )]
    user: Option<String>,
    #[arg(long, help = "Since a given date (yyyy-mm-dd)")]
    date: Option<chrono::NaiveDate>,
    #[arg(
        long,
        short,
        conflicts_with = "date",
        help = "Since days ago, defaults to 1"
    )]
    days: Option<u32>,

    #[command(flatten)]
    ollama: OllamaArgs,

    #[clap(long, short, help = "Enable claude code for summarization")]
    claude: bool,

    #[clap(long, short, help = "Pull all repos before checking")]
    pull: bool,
}

#[derive(clap::Args, Debug)]
#[command(next_help_heading = "Ollama options")]
struct OllamaArgs {
    #[arg(long, short, help = "Enable ollama summarization")]
    ollama: bool,

    #[arg(
        long = "ollama-host",
        default_value = "http://localhost",
        help = "Ollama host"
    )]
    host: String,

    #[arg(long = "ollama-port", default_value_t = 11434, help = "Ollama port")]
    port: u16,

    #[arg(
        long = "ollama-model",
        default_value = "qwen3.8:27b-mlx",
        help = "Model to use for summarizing"
    )]
    model: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let yesterday = chrono::Local::now().date_naive() - TimeDelta::days(1);
    let since: NaiveDate = match (cli.date, cli.days) {
        (Some(since), _) => since,
        (_, Some(days)) => chrono::Local::now().date_naive() - TimeDelta::days(days as i64),
        _ => yesterday,
    };
    let since_ts: i64 = since
        .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap())
        .and_utc()
        .timestamp();

    let user = match cli.user {
        Some(user) => user,
        None => git2::Config::open_default()
            .and_then(|cfg| cfg.get_string("user.name"))
            .context("no --user given and git user.name is not configured")?,
    };

    let dir = cli.dir.unwrap_or(PathBuf::from("."));
    let repos = discover(&dir)?;
    if cli.pull {
        pull(&repos);
    }
    let results: Vec<CommitInfo> = repos
        .into_iter()
        .flat_map(|r| match process_repo(&r, &user, since_ts) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Skipping {}: {e:#}", r.path().display());
                Vec::default()
            }
        })
        .collect();

    match (cli.ollama.ollama, cli.claude) {
        (true, _) => {
            termimad::print_text(&prompt(&results, cli.ollama).await?.response);
        }
        (false, true) => {
            termimad::print_text(&claude(&results).await?);
        }
        _ => summary(results),
    };

    Ok(())
}

async fn prompt(
    results: &Vec<CommitInfo>,
    cli_args: OllamaArgs,
) -> anyhow::Result<GenerationResponse> {
    let url = Url::parse(&format!("{}:{}", cli_args.host, cli_args.port))?;
    let ollama = Ollama::from_url(url);
    let prompt = format!(
        "Summarize these git commits for a scrum standup. Group by repo, one short line per theme of work. Format in markdown, special characers as unicode or ascii. Json: {}",
        serde_json::to_string(&results)?
    );

    let res = ollama
        .generate(GenerationRequest::new(cli_args.model, prompt))
        .await;
    Ok(res?)
}

async fn claude(results: &Vec<CommitInfo>) -> anyhow::Result<String> {
    let json = serde_json::to_string(&results)?;

    let mut child = Command::new("claude")
        .args([
            "-p",
            "Summarize these git commits for a scrum standup. \
             Group by repo, one short line per theme of work. Format in markdown, special characers as unicode or ascii",
            "--output-format",
            "text",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    child
        .stdin
        .take()
        .unwrap()
        .write_all(json.as_bytes())
        .await?;
    let out = child.wait_with_output().await?;
    anyhow::ensure!(
        out.status.success(),
        "claude -p failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?)
}

fn summary(results: Vec<CommitInfo>) {
    let summary = results.into_iter().into_group_map_by(|c| c.repo.clone());
    for (k, v) in summary {
        println!("{}", k.blue());
        for c in v {
            let colour = match c.summary {
                m if m.starts_with("feat") => m.green(),
                m if m.starts_with("fix") => m.red(),
                m if m.starts_with("chore") => m.yellow(),
                _ => c.summary.white(),
            };
            println!("\t {}", colour)
        }
    }
}

fn discover(root: &Path) -> anyhow::Result<Vec<Repository>> {
    if let Ok(repo) = Repository::open(root) {
        return Ok(vec![repo]);
    }
    let mut repos = Vec::default();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() && !is_hidden(&path) {
            match Repository::open(&path)
                .with_context(|| format!("Failed to open repository at {:?}", path))
            {
                Ok(repo) => repos.push(repo),
                Err(_) => repos.extend(discover(&path)?),
            }
        }
    }
    Ok(repos)
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|f| f.starts_with("."))
}

#[derive(Serialize, Deserialize)]
struct CommitInfo {
    id: String,
    when: String,
    summary: String,
    message: String,
    repo: String,
}

impl fmt::Debug for CommitInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommitInfo")
            .field("id", &self.id)
            .field("when", &self.when)
            .field("summary", &self.summary)
            .field("repo", &self.repo)
            .finish()
    }
}

fn pull(repos: &Vec<Repository>) {
    use rayon::prelude::*;
    let workdirs: Vec<PathBuf> = repos
        .iter()
        .filter_map(|r| r.workdir().map(Path::to_path_buf))
        .collect();

    workdirs.par_iter().for_each(|dir| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["pull", "--ff-only", "--quiet"])
            .status();
        match status {
            Ok(s) if s.success() => {}
            _ => eprintln!("pull failed in {}", dir.display()),
        }
    });
}

fn process_repo(repo: &Repository, user: &str, since_ts: i64) -> anyhow::Result<Vec<CommitInfo>> {
    let mut revwalk = repo.revwalk()?;
    revwalk.push_glob("refs/heads/*")?;
    revwalk.push_glob("refs/remotes/*")?;

    let mut res: Vec<CommitInfo> = Vec::new();
    for rev in revwalk {
        let oid = rev?;
        let commit = repo.find_commit(oid)?;
        let commit_ts = commit.time().seconds();
        if commit_ts < since_ts {
            continue;
        }
        let summary = commit.summary()?.unwrap_or("");
        let message = commit.message().unwrap_or("");
        let author = commit.author();
        if !author.name()?.contains(user) {
            continue;
        }

        let datetime = DateTime::from_timestamp(commit.time().seconds(), 0).unwrap();
        res.push(CommitInfo {
            id: commit.id().to_string(),
            when: datetime.format("%Y-%m-%d %H:%M:%S").to_string(),
            summary: summary.to_string(),
            message: message.to_string(),
            repo: repo.path().to_string_lossy().to_string(),
        });
    }
    Ok(res)
}
