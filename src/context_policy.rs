//! Per-session context lifecycle policy.
//!
//! Global handover settings remain the backwards-compatible fallback. More
//! specific [[context_policy]] rules can override individual fields by
//! account, provider, model or session. Automatic compaction is disabled only
//! when the resolved policy has a known context ceiling and enough room for
//! Toomux to hand over before that ceiling.

use crate::config::Config;
use crate::registry::Session;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const POLICY_ENV: &str = "TOOMUX_CONTEXT_POLICY";
pub const MODEL_ENV: &str = "TOOMUX_CONTEXT_MODEL";
pub const PROVIDER_ENV: &str = "TOOMUX_CONTEXT_PROVIDER";
pub const USER_SETTINGS_ENV: &str = "TOOMUX_CONTEXT_USER_SETTINGS";
pub const DEFAULT_SAFETY_RESERVE_TOKENS: u64 = 8_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionMode {
    #[default]
    Auto,
    Native,
    Toomux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionOwner {
    Native,
    Toomux,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContextPolicyRule {
    pub account: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub session: Option<String>,
    pub context_window_tokens: Option<u64>,
    pub handover_turn_end_tokens: Option<u64>,
    pub handover_tokens: Option<u64>,
    pub subagent_handover_tokens: Option<u64>,
    pub fork_context_tokens: Option<u64>,
    pub safety_reserve_tokens: Option<u64>,
    pub compaction: Option<CompactionMode>,
}

#[derive(Debug, Clone, Default)]
pub struct PolicySubject {
    pub account: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedContextPolicy {
    pub account: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub session: Option<String>,
    pub context_window_tokens: Option<u64>,
    pub handover_turn_end_tokens: u64,
    pub handover_tokens: u64,
    pub subagent_handover_tokens: u64,
    pub fork_context_tokens: u64,
    pub safety_reserve_tokens: u64,
    pub requested_compaction: CompactionMode,
    pub compaction_owner: CompactionOwner,
    pub compaction_reason: String,
    pub matched_rules: Vec<String>,
}

impl ResolvedContextPolicy {
    pub fn turn_end_limit(&self) -> u64 {
        match (self.handover_tokens, self.handover_turn_end_tokens) {
            (0, _) => 0,
            (hard, 0) => hard,
            (hard, soft) => soft.min(hard),
        }
    }

    pub fn subagent_limit(&self) -> u64 {
        match (self.handover_tokens, self.subagent_handover_tokens) {
            (0, _) => 0,
            (hard, 0) => hard,
            (hard, sub) => sub.min(hard),
        }
    }

    pub fn owns_compaction(&self) -> bool {
        self.compaction_owner == CompactionOwner::Toomux
    }

    pub fn summary(&self) -> String {
        let capacity = self
            .context_window_tokens
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".into());
        let matched = if self.matched_rules.is_empty() {
            "global defaults".into()
        } else {
            self.matched_rules.join(" -> ")
        };
        format!(
            "context window: {capacity}\nsoft handover: {}\nhard handover: {}\nsubagent: {}\nfork: {}\ncompaction owner: {}\nmatched: {matched}\nreason: {}",
            self.turn_end_limit(),
            self.handover_tokens,
            self.subagent_limit(),
            self.fork_context_tokens,
            match self.compaction_owner {
                CompactionOwner::Native => "native",
                CompactionOwner::Toomux => "toomux",
            },
            self.compaction_reason
        )
    }
}

pub fn resolve(cfg: &Config, mut subject: PolicySubject) -> ResolvedContextPolicy {
    if subject.provider.is_none() {
        subject.provider = subject.model.as_deref().and_then(provider_for_model);
    }
    let mut out = ResolvedContextPolicy {
        account: subject.account.clone(),
        provider: subject.provider.clone(),
        model: subject.model.clone(),
        session: subject.session.clone(),
        context_window_tokens: None,
        handover_turn_end_tokens: cfg.handover_turn_end_tokens,
        handover_tokens: cfg.handover_tokens,
        subagent_handover_tokens: cfg.subagent_handover_tokens,
        fork_context_tokens: cfg.fork_context_tokens,
        safety_reserve_tokens: DEFAULT_SAFETY_RESERVE_TOKENS,
        requested_compaction: CompactionMode::Auto,
        compaction_owner: CompactionOwner::Native,
        compaction_reason: String::new(),
        matched_rules: Vec::new(),
    };

    let mut matching: Vec<(usize, usize, &ContextPolicyRule)> = cfg
        .context_policy
        .iter()
        .enumerate()
        .filter_map(|(i, rule)| {
            rule_matches(rule, &subject).then_some((specificity(rule), i, rule))
        })
        .collect();
    matching.sort_by_key(|(specificity, index, _)| (*specificity, *index));
    for (_, i, rule) in matching {
        apply_rule(&mut out, rule);
        out.matched_rules.push(rule_label(i, rule));
    }

    let safe = out.handover_tokens > 0
        && out.context_window_tokens.is_some_and(|capacity| {
            out.handover_tokens
                .saturating_add(out.safety_reserve_tokens)
                < capacity
        });
    out.compaction_owner = match out.requested_compaction {
        CompactionMode::Native => CompactionOwner::Native,
        CompactionMode::Toomux | CompactionMode::Auto if safe => CompactionOwner::Toomux,
        CompactionMode::Toomux | CompactionMode::Auto => CompactionOwner::Native,
    };
    out.compaction_reason = match (out.requested_compaction, safe, out.context_window_tokens) {
        (CompactionMode::Native, _, _) => {
            "policy explicitly leaves automatic compaction to the client".into()
        }
        (_, true, Some(capacity)) => format!(
            "hard handover {} + reserve {} is below known capacity {capacity}",
            out.handover_tokens, out.safety_reserve_tokens
        ),
        (_, _, None) => "context capacity is unknown; native compaction stays enabled".into(),
        (_, _, Some(capacity)) if out.handover_tokens == 0 => {
            format!("handover is disabled; native compaction protects the {capacity} token context")
        }
        (_, _, Some(capacity)) => format!(
            "hard handover {} + reserve {} is not safely below capacity {capacity}; native compaction stays enabled",
            out.handover_tokens, out.safety_reserve_tokens
        ),
    };
    out
}

fn apply_rule(out: &mut ResolvedContextPolicy, rule: &ContextPolicyRule) {
    if let Some(v) = rule.context_window_tokens {
        out.context_window_tokens = Some(v);
    }
    if let Some(v) = rule.handover_turn_end_tokens {
        out.handover_turn_end_tokens = v;
    }
    if let Some(v) = rule.handover_tokens {
        out.handover_tokens = v;
    }
    if let Some(v) = rule.subagent_handover_tokens {
        out.subagent_handover_tokens = v;
    }
    if let Some(v) = rule.fork_context_tokens {
        out.fork_context_tokens = v;
    }
    if let Some(v) = rule.safety_reserve_tokens {
        out.safety_reserve_tokens = v;
    }
    if let Some(v) = rule.compaction {
        out.requested_compaction = v;
    }
}

fn rule_matches(rule: &ContextPolicyRule, subject: &PolicySubject) -> bool {
    field_matches(rule.account.as_deref(), subject.account.as_deref())
        && field_matches(rule.provider.as_deref(), subject.provider.as_deref())
        && field_matches(rule.model.as_deref(), subject.model.as_deref())
        && field_matches(rule.session.as_deref(), subject.session.as_deref())
}

fn field_matches(pattern: Option<&str>, value: Option<&str>) -> bool {
    match pattern {
        None => true,
        Some(pattern) => value.is_some_and(|value| wildcard(pattern, value)),
    }
}

fn specificity(rule: &ContextPolicyRule) -> usize {
    usize::from(rule.account.is_some())
        + usize::from(rule.provider.is_some()) * 2
        + usize::from(rule.model.is_some()) * 2
        + usize::from(rule.session.is_some()) * 16
}

fn rule_label(index: usize, rule: &ContextPolicyRule) -> String {
    let mut parts = Vec::new();
    for (name, value) in [
        ("account", rule.account.as_deref()),
        ("provider", rule.provider.as_deref()),
        ("model", rule.model.as_deref()),
        ("session", rule.session.as_deref()),
    ] {
        if let Some(value) = value {
            parts.push(format!("{name}={value}"));
        }
    }
    if parts.is_empty() {
        format!("context_policy[{index}]")
    } else {
        format!("context_policy[{index}]({})", parts.join(","))
    }
}

/// Tiny glob matcher for config identifiers. * matches any run and ? one
/// character; matching is ASCII-case-insensitive because account/model IDs are.
fn wildcard(pattern: &str, value: &str) -> bool {
    let p = pattern.to_ascii_lowercase().into_bytes();
    let v = value.to_ascii_lowercase().into_bytes();
    let (mut pi, mut vi, mut star, mut retry) = (0usize, 0usize, None, 0usize);
    while vi < v.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            retry = vi;
        } else if let Some(s) = star {
            pi = s + 1;
            retry += 1;
            vi = retry;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn provider_for_model(model: &str) -> Option<String> {
    if model.starts_with("claude-") {
        Some("anthropic".into())
    } else if model.eq_ignore_ascii_case("chatgpt-browser") {
        Some("chatgpt-browser".into())
    } else if model.starts_with("local-") {
        Some("local".into())
    } else if !model.is_empty() {
        Some("custom".into())
    } else {
        None
    }
}

fn env_value(env: &[(String, String)], name: &str) -> Option<String> {
    env.iter().find_map(|(k, v)| (k == name).then(|| v.clone()))
}

pub fn model_arg(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if let Some(value) = arg.strip_prefix("--model=") {
            return Some(value.to_string());
        }
        if arg == "--model" {
            return it.next().cloned();
        }
    }
    None
}

pub fn for_session(cfg: &Config, session: &Session) -> ResolvedContextPolicy {
    let info = crate::usage::sessions();
    let model = info
        .get(&session.id)
        .and_then(|i| i.model_id.clone())
        .or_else(|| env_value(&session.env, MODEL_ENV))
        .or_else(|| env_value(&session.env, "ANTHROPIC_MODEL"))
        .or_else(|| model_arg(&session.args));
    let provider = env_value(&session.env, PROVIDER_ENV)
        .or_else(|| model.as_deref().and_then(provider_for_model));
    resolve(
        cfg,
        PolicySubject {
            account: session.account.map(|i| cfg.accounts[i].name.clone()),
            provider,
            model,
            session: Some(session.id.clone()),
        },
    )
}

pub fn for_hook(cfg: &Config, v: &Value) -> ResolvedContextPolicy {
    let session = v
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cached = crate::usage::sessions();
    let model = v
        .pointer("/model/id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            session
                .as_deref()
                .and_then(|id| cached.get(id)?.model_id.clone())
        })
        .or_else(|| std::env::var(MODEL_ENV).ok())
        .or_else(|| std::env::var("ANTHROPIC_MODEL").ok());
    let provider = std::env::var(PROVIDER_ENV)
        .ok()
        .or_else(|| model.as_deref().and_then(provider_for_model));
    resolve(
        cfg,
        PolicySubject {
            account: hook_account(cfg, v),
            provider,
            model,
            session,
        },
    )
}

fn hook_account(cfg: &Config, v: &Value) -> Option<String> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR")
        && let Some(i) = cfg.account_for(Some(&dir))
    {
        return Some(cfg.accounts[i].name.clone());
    }
    let transcript = v
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(Path::new)?;
    (0..cfg.accounts.len())
        .find(|&i| transcript.starts_with(crate::config::canon(&cfg.account_dir(i))))
        .map(|i| cfg.accounts[i].name.clone())
}

pub fn launch_subject(
    cfg: &Config,
    account: usize,
    args: &[String],
    inherited_env: &[(String, String)],
    session: Option<&str>,
) -> PolicySubject {
    let model = model_arg(args)
        .or_else(|| env_value(inherited_env, MODEL_ENV))
        .or_else(|| env_value(inherited_env, "ANTHROPIC_MODEL"));
    let provider = env_value(inherited_env, PROVIDER_ENV)
        .or_else(|| model.as_deref().and_then(provider_for_model));
    PolicySubject {
        account: cfg.accounts.get(account).map(|a| a.name.clone()),
        provider,
        model,
        session: session.map(str::to_string),
    }
}

pub struct PreparedLaunch {
    pub args: Vec<String>,
    pub policy: ResolvedContextPolicy,
    pub env: Vec<(String, String)>,
}

pub fn prepare_launch(
    cfg: &Config,
    account: usize,
    cwd: &Path,
    args: &[String],
    inherited_env: &[(String, String)],
    session: Option<&str>,
) -> PreparedLaunch {
    let subject = launch_subject(cfg, account, args, inherited_env, session);
    let mut policy = resolve(cfg, subject);
    let prior_user = env_value(inherited_env, USER_SETTINGS_ENV)
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .filter(Value::is_object);
    let (plain_args, sources) = strip_settings(args);

    let user_settings = match prior_user.clone() {
        Some(value) => Some(value),
        None if sources.is_empty() => Some(Value::Object(Map::new())),
        None if sources.len() == 1 => load_settings_source(cwd, &sources[0]),
        None => None,
    };
    let mut prepared_args = args.to_vec();
    let mut saved_user = None;
    if policy.owns_compaction() {
        match user_settings {
            Some(mut settings) => {
                saved_user = serde_json::to_string(&settings).ok();
                settings
                    .as_object_mut()
                    .expect("settings object checked")
                    .insert("autoCompactEnabled".into(), Value::Bool(false));
                prepared_args = plain_args;
                prepared_args.extend([
                    "--settings".into(),
                    serde_json::to_string(&settings).unwrap_or_else(|_| "{}".into()),
                ]);
            }
            None => {
                policy.compaction_owner = CompactionOwner::Native;
                policy.compaction_reason = if sources.len() > 1 {
                    "multiple --settings sources have unknown client precedence; native compaction stays enabled".into()
                } else {
                    "the existing --settings source could not be read as a JSON object; native compaction stays enabled".into()
                };
            }
        }
    } else if let Some(settings) = prior_user {
        prepared_args = plain_args;
        if settings.as_object().is_some_and(|o| !o.is_empty()) {
            prepared_args.extend([
                "--settings".into(),
                serde_json::to_string(&settings).unwrap_or_else(|_| "{}".into()),
            ]);
        }
        saved_user = serde_json::to_string(&settings).ok();
    }

    let mut env = vec![(
        POLICY_ENV.to_string(),
        serde_json::to_string(&policy).unwrap_or_default(),
    )];
    if let Some(model) = &policy.model {
        env.push((MODEL_ENV.into(), model.clone()));
    }
    if let Some(provider) = &policy.provider {
        env.push((PROVIDER_ENV.into(), provider.clone()));
    }
    if let Some(settings) = saved_user {
        env.push((USER_SETTINGS_ENV.into(), settings));
    }
    PreparedLaunch {
        args: prepared_args,
        policy,
        env,
    }
}

fn strip_settings(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut plain = Vec::new();
    let mut sources = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if let Some(source) = arg.strip_prefix("--settings=") {
            sources.push(source.to_string());
            continue;
        }
        if arg == "--settings" {
            if let Some(source) = it.next() {
                sources.push(source.clone());
            }
            continue;
        }
        plain.push(arg.clone());
    }
    (plain, sources)
}

fn load_settings_source(cwd: &Path, source: &str) -> Option<Value> {
    let raw = if source.trim_start().starts_with('{') {
        source.to_string()
    } else {
        let path = PathBuf::from(source);
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        std::fs::read_to_string(path).ok()?
    };
    serde_json::from_str::<Value>(&raw)
        .ok()
        .filter(Value::is_object)
}

pub fn launch_policy_from_env() -> Option<ResolvedContextPolicy> {
    std::env::var(POLICY_ENV)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

pub fn needs_policy_boundary(current: &ResolvedContextPolicy) -> bool {
    launch_policy_from_env().is_some_and(|launched| {
        launched.compaction_owner != current.compaction_owner
            || (current.handover_tokens > 0 && launched.handover_tokens > current.handover_tokens)
    })
}

pub fn launch_policy_for_session(session: &Session) -> Option<ResolvedContextPolicy> {
    env_value(&session.env, POLICY_ENV).and_then(|raw| serde_json::from_str(&raw).ok())
}

pub fn needs_policy_boundary_for_session(
    session: &Session,
    current: &ResolvedContextPolicy,
) -> bool {
    launch_policy_for_session(session).is_some_and(|launched| {
        launched.compaction_owner != current.compaction_owner
            || (current.handover_tokens > 0 && launched.handover_tokens > current.handover_tokens)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn wildcard_matches_model_families() {
        assert!(wildcard("local-qwen-*", "local-qwen-32b"));
        assert!(wildcard("CLAUDE-*", "claude-opus-5-5"));
        assert!(!wildcard("local-qwen-*", "local-llama-8b"));
    }

    #[test]
    fn more_specific_rules_override_only_their_fields() {
        let mut cfg = Config::default();
        cfg.context_policy = vec![
            ContextPolicyRule {
                account: Some("personal".into()),
                handover_turn_end_tokens: Some(80_000),
                ..Default::default()
            },
            ContextPolicyRule {
                model: Some("local-qwen-*".into()),
                context_window_tokens: Some(32_768),
                handover_tokens: Some(20_000),
                subagent_handover_tokens: Some(12_000),
                fork_context_tokens: Some(10_000),
                compaction: Some(CompactionMode::Toomux),
                ..Default::default()
            },
            ContextPolicyRule {
                account: Some("personal".into()),
                model: Some("local-qwen-*".into()),
                handover_turn_end_tokens: Some(12_000),
                ..Default::default()
            },
        ];
        let p = resolve(
            &cfg,
            PolicySubject {
                account: Some("personal".into()),
                model: Some("local-qwen-32b".into()),
                ..Default::default()
            },
        );
        assert_eq!(p.turn_end_limit(), 12_000);
        assert_eq!(p.handover_tokens, 20_000);
        assert_eq!(p.subagent_limit(), 12_000);
        assert_eq!(p.fork_context_tokens, 10_000);
        assert_eq!(p.compaction_owner, CompactionOwner::Toomux);
        assert_eq!(p.matched_rules.len(), 3);
    }

    #[test]
    fn unknown_or_unsafe_capacity_fails_safe_to_native() {
        let mut cfg = Config::default();
        cfg.context_policy = vec![ContextPolicyRule {
            model: Some("small".into()),
            handover_tokens: Some(30_000),
            context_window_tokens: Some(32_000),
            compaction: Some(CompactionMode::Toomux),
            ..Default::default()
        }];
        let unsafe_policy = resolve(
            &cfg,
            PolicySubject {
                model: Some("small".into()),
                ..Default::default()
            },
        );
        assert_eq!(unsafe_policy.compaction_owner, CompactionOwner::Native);

        cfg.context_policy[0].context_window_tokens = None;
        let unknown = resolve(
            &cfg,
            PolicySubject {
                model: Some("small".into()),
                ..Default::default()
            },
        );
        assert_eq!(unknown.compaction_owner, CompactionOwner::Native);
    }

    #[test]
    fn browser_and_local_models_can_have_independent_concurrent_policies() {
        let mut cfg = Config::default();
        cfg.context_policy = vec![
            ContextPolicyRule {
                account: Some("bonnie".into()),
                context_window_tokens: Some(1_000_000),
                handover_turn_end_tokens: Some(200_000),
                handover_tokens: Some(400_000),
                compaction: Some(CompactionMode::Toomux),
                ..Default::default()
            },
            ContextPolicyRule {
                model: Some("local-qwen-32b".into()),
                context_window_tokens: Some(32_768),
                handover_turn_end_tokens: Some(12_000),
                handover_tokens: Some(20_000),
                subagent_handover_tokens: Some(12_000),
                fork_context_tokens: Some(10_000),
                compaction: Some(CompactionMode::Toomux),
                ..Default::default()
            },
            ContextPolicyRule {
                model: Some("local-llama-128k".into()),
                context_window_tokens: Some(131_072),
                handover_turn_end_tokens: Some(60_000),
                handover_tokens: Some(90_000),
                compaction: Some(CompactionMode::Toomux),
                ..Default::default()
            },
        ];
        let browser = resolve(
            &cfg,
            PolicySubject {
                account: Some("bonnie".into()),
                model: Some("chatgpt-browser".into()),
                ..Default::default()
            },
        );
        let small = resolve(
            &cfg,
            PolicySubject {
                model: Some("local-qwen-32b".into()),
                ..Default::default()
            },
        );
        let large = resolve(
            &cfg,
            PolicySubject {
                model: Some("local-llama-128k".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            (browser.turn_end_limit(), browser.handover_tokens),
            (200_000, 400_000)
        );
        assert_eq!(
            (small.turn_end_limit(), small.handover_tokens),
            (12_000, 20_000)
        );
        assert_eq!(
            (large.turn_end_limit(), large.handover_tokens),
            (60_000, 90_000)
        );
        assert!(browser.owns_compaction() && small.owns_compaction() && large.owns_compaction());
    }

    #[test]
    fn reducing_the_safe_policy_requires_a_fresh_launch_boundary() {
        let launched = ResolvedContextPolicy {
            account: None,
            provider: Some("custom".into()),
            model: Some("large".into()),
            session: Some("s".into()),
            context_window_tokens: Some(1_000_000),
            handover_turn_end_tokens: 200_000,
            handover_tokens: 400_000,
            subagent_handover_tokens: 250_000,
            fork_context_tokens: 200_000,
            safety_reserve_tokens: 8_000,
            requested_compaction: CompactionMode::Toomux,
            compaction_owner: CompactionOwner::Toomux,
            compaction_reason: "test".into(),
            matched_rules: vec![],
        };
        let mut current = launched.clone();
        current.model = Some("small".into());
        current.context_window_tokens = Some(32_768);
        current.handover_tokens = 20_000;
        current.handover_turn_end_tokens = 12_000;
        let raw = serde_json::to_string(&launched).unwrap();
        let session = Session {
            pid: 1,
            proc_start: None,
            id: "s".into(),
            cwd: "/tmp".into(),
            name: "s".into(),
            title: "s".into(),
            topic: None,
            pr: None,
            queued: None,
            pin: None,
            dormant: false,
            restore: None,
            state: crate::registry::State::Idle,
            waiting_for: None,
            limit: None,
            handover: None,
            since_ms: 0,
            started_ms: 0,
            account: None,
            config_dir: None,
            args: vec![],
            env: vec![(POLICY_ENV.into(), raw)],
            tty: None,
            pane: None,
        };
        assert!(needs_policy_boundary_for_session(&session, &current));
    }

    #[test]
    fn launch_overlay_preserves_user_settings_without_writing_their_file() {
        let tmp =
            std::env::temp_dir().join(format!("toomux-context-settings-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(
            tmp.join("settings.json"),
            r#"{"permissions":{"defaultMode":"default"},"autoCompactEnabled":true}"#,
        )
        .unwrap();
        let mut cfg = Config::default();
        cfg.accounts = vec![crate::config::Account {
            name: "local".into(),
            config_dir: "/tmp/local".into(),
        }];
        cfg.context_policy = vec![ContextPolicyRule {
            account: Some("local".into()),
            context_window_tokens: Some(128_000),
            handover_tokens: Some(90_000),
            compaction: Some(CompactionMode::Toomux),
            ..Default::default()
        }];
        let args = vec![
            "--settings".into(),
            "settings.json".into(),
            "--model".into(),
            "x".into(),
        ];
        let prepared = prepare_launch(&cfg, 0, &tmp, &args, &[], None);
        assert!(prepared.policy.owns_compaction());
        assert_eq!(prepared.args[0], "--model");
        let json: Value = serde_json::from_str(prepared.args.last().unwrap()).unwrap();
        assert_eq!(json["autoCompactEnabled"], false);
        assert_eq!(json["permissions"]["defaultMode"], "default");
        assert_eq!(
            std::fs::read_to_string(tmp.join("settings.json")).unwrap(),
            r#"{"permissions":{"defaultMode":"default"},"autoCompactEnabled":true}"#
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn leaving_toomux_ownership_removes_only_the_toomux_overlay() {
        let mut cfg = Config::default();
        cfg.accounts = vec![crate::config::Account {
            name: "personal".into(),
            config_dir: "/tmp/personal".into(),
        }];
        cfg.context_policy = vec![ContextPolicyRule {
            account: Some("personal".into()),
            context_window_tokens: Some(128_000),
            handover_tokens: Some(90_000),
            compaction: Some(CompactionMode::Native),
            ..Default::default()
        }];
        let original = serde_json::json!({"permissions":{"defaultMode":"plan"}});
        let managed = serde_json::json!({
            "permissions":{"defaultMode":"plan"},
            "autoCompactEnabled":false
        });
        let args = vec![
            "--model".into(),
            "claude-test".into(),
            "--settings".into(),
            managed.to_string(),
        ];
        let inherited = vec![(
            USER_SETTINGS_ENV.into(),
            serde_json::to_string(&original).unwrap(),
        )];
        let prepared = prepare_launch(&cfg, 0, Path::new("/tmp"), &args, &inherited, Some("s"));
        assert_eq!(prepared.policy.compaction_owner, CompactionOwner::Native);
        let source = prepared.args.last().unwrap();
        let restored: Value = serde_json::from_str(source).unwrap();
        assert_eq!(restored["permissions"]["defaultMode"], "plan");
        assert!(restored.get("autoCompactEnabled").is_none());
    }

    #[test]
    fn multiple_user_settings_sources_fail_safe_without_rewriting_args() {
        let mut cfg = Config::default();
        cfg.accounts = vec![crate::config::Account {
            name: "local".into(),
            config_dir: "/tmp/local".into(),
        }];
        cfg.context_policy = vec![ContextPolicyRule {
            account: Some("local".into()),
            context_window_tokens: Some(128_000),
            handover_tokens: Some(90_000),
            compaction: Some(CompactionMode::Toomux),
            ..Default::default()
        }];
        let args = vec![
            "--settings".into(),
            "one.json".into(),
            "--settings".into(),
            "two.json".into(),
        ];
        let prepared = prepare_launch(&cfg, 0, Path::new("/tmp"), &args, &[], Some("s"));
        assert_eq!(prepared.policy.compaction_owner, CompactionOwner::Native);
        assert_eq!(prepared.args, args);
        assert!(
            prepared
                .policy
                .compaction_reason
                .contains("multiple --settings")
        );
    }

    #[test]
    fn handover_disabled_never_claims_compaction_ownership() {
        let mut cfg = Config {
            handover_tokens: 0,
            ..Config::default()
        };
        cfg.context_policy = vec![ContextPolicyRule {
            model: Some("large".into()),
            context_window_tokens: Some(1_000_000),
            compaction: Some(CompactionMode::Toomux),
            ..Default::default()
        }];
        let policy = resolve(
            &cfg,
            PolicySubject {
                model: Some("large".into()),
                ..Default::default()
            },
        );
        assert_eq!(policy.compaction_owner, CompactionOwner::Native);
    }
}
