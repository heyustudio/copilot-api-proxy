//! `claude-setup` subcommand: discover the native Claude models that actually
//! work through the Copilot `/v1/messages` passthrough, let the user pick one
//! per Claude Code slot, and write an executable launcher script.

use std::cmp::Reverse;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Bytes;
use futures::stream::{self, StreamExt};
use reqwest::Method;

use crate::auth::TokenManager;
use crate::claude::is_native_claude_model;
use crate::config;
use crate::proxy::ProxyClient;

/// Options for [`run`], populated from the `claude-setup` CLI args.
pub struct SetupOptions {
    /// Path to write the generated launcher script.
    pub output: PathBuf,
    /// Port the launcher should target (`http://localhost:<port>`).
    pub port: u16,
    /// Probe each candidate with a `/v1/messages` call to confirm it works.
    pub probe: bool,
    /// Include `--dangerously-skip-permissions` in the launcher.
    pub skip_permissions: bool,
    /// Accept recommended defaults without prompting.
    pub assume_yes: bool,
}

/// Default launcher location: `~/.local/bin/claude-proxy`.
pub fn default_output_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/bin/claude-proxy")
}

/// A native Claude model offered by the Copilot catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Candidate {
    id: String,
    reasoning_efforts: Vec<String>,
}

impl Candidate {
    fn supports_effort(&self) -> bool {
        !self.reasoning_efforts.is_empty()
    }
}

/// The three Claude Code model slots.
#[derive(Clone, Copy)]
enum Slot {
    Opus,
    Sonnet,
    Haiku,
}

impl Slot {
    fn keyword(self) -> &'static str {
        match self {
            Slot::Opus => "opus",
            Slot::Sonnet => "sonnet",
            Slot::Haiku => "haiku",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Slot::Opus => "Opus",
            Slot::Sonnet => "Sonnet",
            Slot::Haiku => "Haiku",
        }
    }

    fn env(self) -> &'static str {
        match self {
            Slot::Opus => "ANTHROPIC_DEFAULT_OPUS_MODEL",
            Slot::Sonnet => "ANTHROPIC_DEFAULT_SONNET_MODEL",
            Slot::Haiku => "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        }
    }
}

/// Entry point for `copilot-api-proxy claude-setup`.
pub async fn run(opts: SetupOptions) -> Result<()> {
    let proxy = build_proxy().await?;

    println!("Fetching available models from Copilot…");
    let mut candidates = discover_native_claude_models(&proxy).await?;
    if candidates.is_empty() {
        anyhow::bail!("No native Claude models found in the Copilot catalog.");
    }

    if opts.probe {
        println!(
            "Probing {} candidate model(s) for /v1/messages support…",
            candidates.len()
        );
        candidates = probe_models(&proxy, candidates).await;
        if candidates.is_empty() {
            anyhow::bail!(
                "No Claude models passed the /v1/messages probe. \
                 Re-run with --no-probe to skip verification."
            );
        }
    }

    // Interactive input is read from /dev/tty so the picker works even when the
    // process was launched via `curl | bash` (stdin is the pipe, not the user).
    let mut tty = if opts.assume_yes {
        None
    } else {
        match tty_reader() {
            Some(reader) => Some(reader),
            None => {
                eprintln!(
                    "No interactive terminal (/dev/tty) available — using recommended defaults."
                );
                None
            }
        }
    };

    let opus = select_slot(Slot::Opus, &candidates, tty.as_mut());
    let sonnet = select_slot(Slot::Sonnet, &candidates, tty.as_mut());
    let haiku = select_haiku(&candidates, &sonnet, tty.as_mut());

    let script = render_script(&ScriptConfig {
        port: opts.port,
        opus: &opus.id,
        sonnet: &sonnet.id,
        haiku: &haiku.id,
        skip_permissions: opts.skip_permissions,
    });
    write_executable(&opts.output, &script)?;

    println!();
    println!("Wrote launcher: {}", opts.output.display());
    println!("  opus   → {}", opus.id);
    println!("  sonnet → {}", sonnet.id);
    println!("  haiku  → {}", haiku.id);
    println!();
    match opts.output.file_name().and_then(|n| n.to_str()) {
        Some(name) if on_path(&opts.output) => println!("Start the proxy, then run: {name}"),
        _ => println!("Start the proxy, then run: {}", opts.output.display()),
    }

    Ok(())
}

/// Build a `ProxyClient` from the stored GitHub token without starting the server.
async fn build_proxy() -> Result<Arc<ProxyClient>> {
    let token = config::load_github_token()
        .context("No GitHub token found — run `copilot-api-proxy auth` first")?;
    let manager = Arc::new(
        TokenManager::new(token)
            .await
            .context("Failed to exchange GitHub token for a Copilot token")?,
    );
    Ok(Arc::new(ProxyClient::new(manager)?))
}

/// GET `/models` and keep the native Claude entries, with reasoning-effort support.
async fn discover_native_claude_models(proxy: &ProxyClient) -> Result<Vec<Candidate>> {
    let resp = proxy
        .forward("/models", Method::GET, Bytes::new(), None, None, false)
        .await
        .context("request to Copilot /models failed")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("Copilot /models returned HTTP {status}");
    }
    let value: serde_json::Value = resp.json().await.context("parsing /models response")?;
    parse_native_claude_models(&value)
}

/// Pure parser split out for unit testing.
fn parse_native_claude_models(value: &serde_json::Value) -> Result<Vec<Candidate>> {
    let data = value
        .get("data")
        .and_then(|v| v.as_array())
        .context("/models response missing data[] array")?;

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for model in data {
        let Some(id) = model.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if !is_native_claude_model(id) || !seen.insert(id.to_string()) {
            continue;
        }
        let reasoning_efforts = model
            .pointer("/capabilities/supports/reasoning_effort")
            .and_then(|v| v.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        out.push(Candidate {
            id: id.to_string(),
            reasoning_efforts,
        });
    }
    Ok(out)
}

/// Probe each candidate with a 1-token `/v1/messages` request; keep HTTP 200s.
async fn probe_models(proxy: &ProxyClient, candidates: Vec<Candidate>) -> Vec<Candidate> {
    const CONCURRENCY: usize = 5;

    let results = stream::iter(candidates.into_iter().map(|candidate| async move {
        let ok = probe_one(proxy, &candidate.id).await;
        (candidate, ok)
    }))
    .buffer_unordered(CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut kept = Vec::new();
    for (candidate, ok) in results {
        if ok {
            kept.push(candidate);
        } else {
            eprintln!("  skip {} (not usable via /v1/messages)", candidate.id);
        }
    }
    // Restore a deterministic order; display/selection sorts again per slot.
    kept.sort_by(|a, b| a.id.cmp(&b.id));
    kept
}

async fn probe_one(proxy: &ProxyClient, model: &str) -> bool {
    // Mirror the runtime path: the /v1/messages handler applies the deprecated-model
    // alias remap before forwarding, so probe the id Copilot will actually receive.
    let upstream_model = crate::llm::remap_model_name(model);
    let body = serde_json::json!({
        "model": upstream_model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let bytes = match serde_json::to_vec(&body) {
        Ok(bytes) => Bytes::from(bytes),
        Err(_) => return false,
    };
    match proxy
        .forward(
            "/v1/messages",
            Method::POST,
            bytes,
            Some("application/json"),
            Some("user"),
            false,
        )
        .await
    {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// Ordering key: effort support first, then version, then prefer the shorter /
/// canonical id. Larger key = better.
fn rank_key(candidate: &Candidate) -> (bool, (u32, u32), Reverse<usize>, Reverse<String>) {
    (
        candidate.supports_effort(),
        parse_version(&candidate.id),
        Reverse(candidate.id.len()),
        Reverse(candidate.id.clone()),
    )
}

/// Extract a `major.minor` version from a model id, e.g. `claude-opus-4.7` → (4, 7).
fn parse_version(id: &str) -> (u32, u32) {
    for segment in id.split(['-', '_']) {
        if let Some((major, minor)) = segment.split_once('.')
            && let (Ok(major), Ok(minor)) = (major.parse::<u32>(), minor.parse::<u32>())
        {
            return (major, minor);
        }
    }
    (0, 0)
}

/// Candidates whose id matches `keyword`, sorted best-first. Falls back to the
/// full list if nothing matches so the user can always pick something.
fn candidates_for(keyword: &str, all: &[Candidate]) -> Vec<Candidate> {
    let mut matches: Vec<Candidate> = all
        .iter()
        .filter(|c| c.id.to_lowercase().contains(keyword))
        .cloned()
        .collect();
    if matches.is_empty() {
        matches = all.to_vec();
    }
    // Best-first: rank_key is "higher = better", so sort by its reverse.
    matches.sort_by_key(|c| Reverse(rank_key(c)));
    matches
}

fn select_slot(slot: Slot, all: &[Candidate], tty: Option<&mut BufReader<File>>) -> Candidate {
    let options = candidates_for(slot.keyword(), all);
    let idx = prompt_choice(slot, &options, 0, tty);
    options[idx].clone()
}

/// Haiku is special: Claude Code sends a reasoning effort on background calls,
/// which models without effort support reject. Default to the best effort-capable
/// haiku model, or the chosen Sonnet model if no haiku supports effort.
fn select_haiku(
    all: &[Candidate],
    sonnet: &Candidate,
    tty: Option<&mut BufReader<File>>,
) -> Candidate {
    let mut options = candidates_for("haiku", all);
    let default_idx = match options.iter().position(Candidate::supports_effort) {
        Some(idx) => idx,
        None => {
            if !options.iter().any(|c| c.id == sonnet.id) {
                options.push(sonnet.clone());
            }
            options
                .iter()
                .position(|c| c.id == sonnet.id)
                .unwrap_or(0)
        }
    };
    let idx = prompt_choice(Slot::Haiku, &options, default_idx, tty);
    options[idx].clone()
}

/// Print a numbered menu and read a selection from `/dev/tty`. Returns
/// `default_idx` on empty input, EOF, or when no terminal is available.
fn prompt_choice(
    slot: Slot,
    options: &[Candidate],
    default_idx: usize,
    tty: Option<&mut BufReader<File>>,
) -> usize {
    let Some(tty) = tty else {
        return default_idx;
    };

    println!("\nSelect {} model ({}):", slot.label(), slot.env());
    for (i, candidate) in options.iter().enumerate() {
        let marker = if i == default_idx { '*' } else { ' ' };
        let effort = if candidate.supports_effort() {
            format!("reasoning effort: {}", candidate.reasoning_efforts.join(", "))
        } else {
            "no reasoning effort".to_string()
        };
        println!("  {marker} [{}] {}  ({effort})", i + 1, candidate.id);
    }

    loop {
        print!("Choice [default {}]: ", default_idx + 1);
        let _ = std::io::stdout().flush();

        let mut line = String::new();
        if tty.read_line(&mut line).unwrap_or(0) == 0 {
            println!();
            return default_idx;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return default_idx;
        }
        if let Ok(n) = trimmed.parse::<usize>()
            && (1..=options.len()).contains(&n)
        {
            return n - 1;
        }
        println!("  Please enter a number between 1 and {}.", options.len());
    }
}

fn tty_reader() -> Option<BufReader<File>> {
    std::fs::OpenOptions::new()
        .read(true)
        .open("/dev/tty")
        .ok()
        .map(BufReader::new)
}

/// Best-effort check whether `path`'s directory is on `$PATH`.
fn on_path(path: &Path) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|p| p == dir)
}

struct ScriptConfig<'a> {
    port: u16,
    opus: &'a str,
    sonnet: &'a str,
    haiku: &'a str,
    skip_permissions: bool,
}

fn render_script(cfg: &ScriptConfig) -> String {
    let flags = if cfg.skip_permissions {
        " --dangerously-skip-permissions"
    } else {
        ""
    };
    format!(
        r#"#!/usr/bin/env bash
# Generated by `copilot-api-proxy claude-setup`. Re-run that command to refresh
# the model list — manual edits here will be overwritten on the next run.
set -euo pipefail

PROXY_URL="${{PROXY_URL:-http://localhost:{port}}}"

if ! curl -fsS -o /dev/null --max-time 2 "$PROXY_URL/v1/models"; then
  echo "error: proxy not reachable at $PROXY_URL" >&2
  echo "  start it with: copilot-api-proxy server" >&2
  exit 1
fi

unset ANTHROPIC_API_KEY

export ANTHROPIC_BASE_URL="$PROXY_URL"
export ANTHROPIC_AUTH_TOKEN="${{ANTHROPIC_AUTH_TOKEN:-copilot-proxy-local}}"
export ANTHROPIC_DEFAULT_OPUS_MODEL="{opus}"
export ANTHROPIC_DEFAULT_SONNET_MODEL="{sonnet}"
export ANTHROPIC_DEFAULT_HAIKU_MODEL="{haiku}"

exec claude{flags} "$@"
"#,
        port = cfg.port,
        opus = cfg.opus,
        sonnet = cfg.sonnet,
        haiku = cfg.haiku,
        flags = flags,
    )
}

/// Write `content` to `path` atomically (temp file + rename) and chmod 0755.
fn write_executable(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp-claude-setup");
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("setting permissions on {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("moving launcher into place at {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, efforts: &[&str]) -> Candidate {
        Candidate {
            id: id.to_string(),
            reasoning_efforts: efforts.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn parses_native_claude_models_with_efforts() {
        let value = serde_json::json!({
            "data": [
                { "id": "gpt-4o-mini", "capabilities": { "supports": { "reasoning_effort": ["low"] } } },
                { "id": "claude-opus-4.7", "capabilities": { "supports": { "reasoning_effort": ["low", "high"] } } },
                { "id": "claude-haiku-4.5", "capabilities": { "supports": {} } },
                { "id": "claude-opus-4.7" }
            ]
        });
        let models = parse_native_claude_models(&value).unwrap();
        let ids: Vec<&str> = models.iter().map(|c| c.id.as_str()).collect();
        // gpt skipped, claude-opus deduped, haiku has no efforts.
        assert_eq!(ids, vec!["claude-opus-4.7", "claude-haiku-4.5"]);
        assert_eq!(models[0].reasoning_efforts, vec!["low", "high"]);
        assert!(models[1].reasoning_efforts.is_empty());
    }

    #[test]
    fn parse_version_extracts_major_minor() {
        assert_eq!(parse_version("claude-opus-4.7-1m-internal"), (4, 7));
        assert_eq!(parse_version("claude-sonnet-4.6"), (4, 6));
        assert_eq!(parse_version("claude-opus-4.5"), (4, 5));
        assert_eq!(parse_version("weird-name"), (0, 0));
    }

    #[test]
    fn best_candidate_prefers_effort_then_version_then_canonical() {
        let all = vec![
            candidate("claude-opus-4.5", &["high"]),
            candidate("claude-opus-4.7-1m-internal", &["high"]),
            candidate("claude-opus-4.7-xhigh", &["xhigh"]),
            candidate("claude-opus-4.7", &["low", "high"]),
        ];
        let ranked = candidates_for("opus", &all);
        // Highest version (4.7), canonical (shortest) id wins.
        assert_eq!(ranked[0].id, "claude-opus-4.7");
    }

    #[test]
    fn select_slot_defaults_to_best_when_non_interactive() {
        let all = vec![
            candidate("claude-sonnet-4.5", &["high"]),
            candidate("claude-sonnet-4.6", &["high"]),
        ];
        let chosen = select_slot(Slot::Sonnet, &all, None);
        assert_eq!(chosen.id, "claude-sonnet-4.6");
    }

    #[test]
    fn haiku_falls_back_to_sonnet_when_no_effort_support() {
        let all = vec![
            candidate("claude-haiku-4.5", &[]), // no reasoning effort
            candidate("claude-sonnet-4.6", &["high"]),
        ];
        let sonnet = candidate("claude-sonnet-4.6", &["high"]);
        let chosen = select_haiku(&all, &sonnet, None);
        assert_eq!(chosen.id, "claude-sonnet-4.6");
    }

    #[test]
    fn haiku_prefers_effort_capable_haiku_when_available() {
        let all = vec![
            candidate("claude-haiku-4.5", &["low", "high"]),
            candidate("claude-sonnet-4.6", &["high"]),
        ];
        let sonnet = candidate("claude-sonnet-4.6", &["high"]);
        let chosen = select_haiku(&all, &sonnet, None);
        assert_eq!(chosen.id, "claude-haiku-4.5");
    }

    #[test]
    fn render_script_includes_models_and_flags() {
        let script = render_script(&ScriptConfig {
            port: 9876,
            opus: "claude-opus-4.7",
            sonnet: "claude-sonnet-4.6",
            haiku: "claude-sonnet-4.6",
            skip_permissions: true,
        });
        assert!(script.starts_with("#!/usr/bin/env bash"));
        assert!(script.contains("http://localhost:9876"));
        assert!(script.contains(r#"ANTHROPIC_DEFAULT_OPUS_MODEL="claude-opus-4.7""#));
        assert!(script.contains(r#"ANTHROPIC_DEFAULT_HAIKU_MODEL="claude-sonnet-4.6""#));
        assert!(script.contains("exec claude --dangerously-skip-permissions \"$@\""));
        // Bash variable expansion must survive formatting.
        assert!(script.contains(r#""${PROXY_URL:-http://localhost:9876}""#));
    }

    #[test]
    fn render_script_omits_skip_permissions_when_disabled() {
        let script = render_script(&ScriptConfig {
            port: 1234,
            opus: "a",
            sonnet: "b",
            haiku: "c",
            skip_permissions: false,
        });
        assert!(script.contains("exec claude \"$@\""));
        assert!(!script.contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn write_executable_is_atomic_and_marked_executable() {
        let dir = std::env::temp_dir().join(format!("claude-setup-test-{}", std::process::id()));
        let path = dir.join("claude-proxy");
        write_executable(&path, "#!/usr/bin/env bash\n").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("#!/usr/bin/env bash"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
