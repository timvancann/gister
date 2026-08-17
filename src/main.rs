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
use termimad::crossterm::style::Stylize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use clap::Parser;
use serde_json;

const ME: &str = "Tim";

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    dir: Option<PathBuf>,
    #[arg(long, short)]
    user: Option<String>,
    #[arg(long, short, help = "Since a given date")]
    since: Option<chrono::NaiveDate>,
    #[arg(long, short, conflicts_with = "since")]
    days: Option<u32>,

    #[clap(long, short, help = "Send prompt to ollama")]
    llm: bool,
    #[clap(long, short, help = "Send prompt to claude code")]
    claude: bool,

    #[clap(long, short, help = "Pull all repos before checking")]
    pull: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let yesterday = chrono::Local::now().date_naive() - TimeDelta::days(1);
    let since: NaiveDate = match (cli.since, cli.days) {
        (Some(since), _) => since,
        (_, Some(days)) => chrono::Local::now().date_naive() - TimeDelta::days(days as i64),
        _ => yesterday,
    };
    let since_ts: i64 = since
        .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap())
        .and_utc()
        .timestamp();

    let user = cli.user.unwrap_or(ME.to_string());

    let dir = cli.dir.unwrap_or(PathBuf::from("."));
    let mut repos: Vec<Repository> = Vec::default();
    discover(&dir, &mut repos)?;
    if cli.pull {
        pull(&repos);
    }
    let results: Vec<CommitInfo> = repos
        .into_iter()
        .flat_map(|r| process_repo(&r, &user, since_ts).unwrap())
        .collect();

    if cli.llm {
        let res = prompt(results).await?;
        termimad::print_text(&res.response);
    } else if cli.claude {
        termimad::print_text(&claude(results).await?);
    } else {
        summary(results);
    }

    Ok(())
}

async fn prompt(results: Vec<CommitInfo>) -> anyhow::Result<GenerationResponse> {
    let ollama = Ollama::default();
    let model = "qwen3.8:27b-mlx".to_string();
    let prompt = format!(
        "Attached is a json containing commit information. The purpose is to get a small summary of the commits for the purpose of a scrum standup.
        Emit any special character as unicode as this will be printed to a shell as part of a CLI.
        The json is as follows: {}",
        serde_json::to_string(&results).expect("Serialization failed")
    );

    let res = ollama.generate(GenerationRequest::new(model, prompt)).await;
    Ok(res?)
}

async fn claude(results: Vec<CommitInfo>) -> anyhow::Result<String> {
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

fn discover(root: &Path, repos: &mut Vec<Repository>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            match Repository::open(&path)
                .with_context(|| format!("Failed to open repository at {:?}", path))
            {
                Ok(repo) => {
                    repos.push(repo);
                }
                Err(_) => {
                    discover(&path, repos)?;
                }
            }
        }
    }
    Ok(())
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
        let summary = commit.summary()?.unwrap();
        let message = commit.message().unwrap_or("").to_string();
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
