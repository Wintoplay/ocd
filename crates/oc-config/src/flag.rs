//! Feature flags (`schema/src/config/flag.ts`, `core/src/flag.ts`, 2.3.1): one registry of opt-in features, each
//! read with [`enabled`]. A flag is on in local and dev builds and off in released ones; `experimental.<key>` in
//! config and `OPENCODE_FLAGS` override that, and `OPENCODE_FLAGS=all` or `experimental.all_flags: true` turn every
//! flag on except the opt-in-only ones ([`OPT_IN_ONLY`]), which must be named and are off on every channel. An
//! earlier name of a flag ([`ALIASES`]) is still read when the new key is unset.

use indexmap::IndexMap;

use crate::schema::Entry;

/// The `OPENCODE_FLAGS` entry that enables all flags except opt-in-only flags, as in `all,-media_blob`. It is not a
/// flag key.
pub const WILDCARD: &str = "all";

/// Every feature flag, by key, with what it turns on (`ConfigFlag.registry`, in its order).
pub const REGISTRY: &[(&str, &str)] = &[
    ("todo", "Enable the experimental todowrite tool."),
    ("media_blob", "Store new session media once in a deduplicated blob table instead of inline in every message row."),
    (
        "automations",
        "Run a prompt or Code Mode action on a timer, or on a connected MCP server's push event when mcp_events is enabled.",
    ),
    ("artifacts", "Enable experimental artifacts backed by pluggable providers."),
    ("video_generate", "Enable the experimental video generation API."),
    (
        "console_watches",
        "Watch Slack threads, GitHub pull requests, issues, commits and Actions runs, and the workspace's MCP servers' events through OpenCode Console, and wake sessions when they change.",
    ),
    (
        "mcp_events",
        "Enable experimental MCP Events, a draft MCP extension: declare support to MCP servers and read the events of servers that offer them.",
    ),
    (
        "full_codemode",
        "Enable the full Code Mode work in progress. Off by default in every build, including local and dev; opt in with OPENCODE_FLAGS=full_codemode or experimental.full_codemode: true.",
    ),
];

/// `ConfigFlag.optInOnly`: flags that `all` and `all_flags` skip, off by default on every channel.
pub const OPT_IN_ONLY: &[&str] = &["full_codemode"];

/// `ConfigFlag.aliases`: earlier names of a flag, still read from `experimental.<name>` and `OPENCODE_FLAGS` when
/// the new key is unset.
pub const ALIASES: &[(&str, &str)] = &[("schedules", "automations")];

/// Where a flag's value came from, highest priority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Config,
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::Config => "config",
            Source::Default => "default",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    pub enabled: bool,
    pub source: Source,
}

/// Any other channel, such as a release or a branch preview build, keeps flags off so none turns on by accident.
fn on_by_default(channel: &str) -> bool {
    channel == "local" || channel == "dev"
}

/// `Flag.resolve`: `OPENCODE_FLAGS` (`key` or `-key`, then `all`), then `experimental.<key>` from the
/// highest-priority document that sets `experimental`, then `all_flags: true` from the highest-priority document that
/// sets `all_flags` (`false` falls through rather than turning flags off), then on for local and dev builds. A flag's
/// old name counts below its new one at each level; opt-in-only flags skip both wildcards and default to off.
pub fn resolve(entries: &[Entry], channel: &str, overrides: Option<&str>) -> IndexMap<&'static str, State> {
    let mut env: IndexMap<&str, bool> = IndexMap::new();
    for item in overrides.unwrap_or_default().split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match item.strip_prefix('-') {
            Some(key) => env.insert(key, false),
            None => env.insert(item, true),
        };
    }
    let all = env.get(WILDCARD) == Some(&true);
    let experimental = crate::fold::experimental(entries);
    let all_flags =
        crate::fold::latest(entries, |i| i.experimental.as_ref().and_then(|e| e.all_flags.as_ref())) == Some(&true);
    REGISTRY
        .iter()
        .map(|(key, _)| {
            let opt_in_only = OPT_IN_ONLY.contains(key);
            let old = ALIASES.iter().find(|(_, new)| new == key).map(|(old, _)| *old);
            let override_ = env
                .get(key)
                .copied()
                .or_else(|| old.and_then(|o| env.get(o).copied()))
                .or((all && !opt_in_only).then_some(true));
            let configured = experimental
                .and_then(|e| e.flag(key).or_else(|| old.and_then(|o| e.flag(o))))
                .or((all_flags && !opt_in_only).then_some(true));
            let state = match (override_, configured) {
                (Some(enabled), _) => State { enabled, source: Source::Env },
                (None, Some(enabled)) => State { enabled, source: Source::Config },
                (None, None) => State { enabled: !opt_in_only && on_by_default(channel), source: Source::Default },
            };
            (*key, state)
        })
        .collect()
}

/// `Flag.enabled(key)` with this build's channel and the process's `OPENCODE_FLAGS`.
pub fn enabled(entries: &[Entry], key: &str) -> bool {
    let overrides = std::env::var("OPENCODE_FLAGS").ok();
    resolve(entries, oc_util::version::CHANNEL, overrides.as_deref()).get(key).is_some_and(|s| s.enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(experimental: serde_json::Value) -> Entry {
        let info = <crate::schema::Info as crate::decode::Decode>::decode(&json!({ "experimental": experimental })).expect("config");
        Entry::Document { path: None, info, integration: None, wellknown: None }
    }

    fn get(entries: &[Entry], channel: &str, overrides: Option<&str>, key: &str) -> (bool, &'static str) {
        let s = resolve(entries, channel, overrides)[key];
        (s.enabled, s.source.as_str())
    }

    fn implicit(enabled: bool, source: Source) -> IndexMap<&'static str, State> {
        REGISTRY
            .iter()
            .map(|(k, _)| {
                let state = if OPT_IN_ONLY.contains(k) { State { enabled: false, source: Source::Default } } else { State { enabled, source } };
                (*k, state)
            })
            .collect()
    }

    fn with(mut map: IndexMap<&'static str, State>, key: &'static str, enabled: bool, source: Source) -> IndexMap<&'static str, State> {
        map.insert(key, State { enabled, source });
        map
    }

    #[test]
    fn registry_order_and_retired_keys() {
        assert_eq!(
            REGISTRY.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            ["todo", "media_blob", "automations", "artifacts", "video_generate", "console_watches", "mcp_events", "full_codemode"]
        );
        assert_eq!(resolve(&[], "latest", None).keys().copied().collect::<Vec<_>>(), REGISTRY.iter().map(|(k, _)| *k).collect::<Vec<_>>());
        // 2.3.1 retires these two flags (their behaviour is the default now) and `codemode_vm` (2.3.0).
        for gone in ["background_compaction", "shell_timeout_background", "schedules", "codemode_vm"] {
            assert!(!resolve(&[], "dev", Some(gone)).contains_key(gone), "{gone}");
        }
    }

    /// `flag.test.ts` (2.3.1), case by case.
    #[test]
    fn applies_build_defaults_and_leaves_full_codemode_off() {
        for (channel, on) in [("local", true), ("dev", true), ("beta", false), ("latest", false), ("my-branch", false), ("unknown", false)] {
            assert_eq!(resolve(&[], channel, None), implicit(on, Source::Default), "{channel}");
        }
    }

    #[test]
    fn full_codemode_can_be_enabled_or_disabled_by_name() {
        let entries = [doc(json!({"full_codemode": true}))];
        assert_eq!(get(&[], "latest", Some("full_codemode"), "full_codemode"), (true, "env"));
        assert_eq!(get(&entries, "latest", None, "full_codemode"), (true, "config"));
        assert_eq!(get(&entries, "latest", Some("-full_codemode"), "full_codemode"), (false, "env"));
        assert_eq!(get(&[doc(json!({"full_codemode": false}))], "latest", Some("all"), "full_codemode"), (false, "config"));
        // Not on in local and dev builds either, and `all_flags` skips it.
        assert_eq!(get(&[], "local", None, "full_codemode"), (false, "default"));
        assert_eq!(get(&[doc(json!({"all_flags": true}))], "dev", None, "full_codemode"), (false, "default"));
    }

    #[test]
    fn config_overrides_the_default_and_opencode_flags_overrides_config() {
        let flags = resolve(&[doc(json!({"todo": true, "automations": false}))], "dev", Some(" media_blob , -todo,unknown_flag"));
        assert_eq!(
            ["todo", "automations", "media_blob", "artifacts"].map(|k| (flags[k].enabled, flags[k].source)),
            [(false, Source::Env), (false, Source::Config), (true, Source::Env), (true, Source::Default)]
        );
    }

    #[test]
    fn reads_the_old_schedules_key_for_automations_below_the_new_key() {
        assert_eq!(get(&[doc(json!({"schedules": true}))], "latest", None, "automations"), (true, "config"));
        assert_eq!(get(&[doc(json!({"schedules": true, "automations": false}))], "latest", None, "automations"), (false, "config"));
        assert_eq!(get(&[], "latest", Some("schedules"), "automations"), (true, "env"));
        assert_eq!(get(&[], "latest", Some("schedules,-automations"), "automations"), (false, "env"));
        assert_eq!(get(&[doc(json!({"automations": true}))], "latest", Some("-schedules"), "automations"), (false, "env"));
    }

    #[test]
    fn reads_experimental_from_the_highest_priority_document_that_sets_it() {
        let shadowed = [doc(json!({"todo": true})), doc(json!({"subagent_depth": 2})), doc(json!({}))];
        assert_eq!(get(&shadowed, "latest", None, "todo"), (false, "default"));
        let on = [doc(json!({"todo": true}))];
        assert_eq!(get(&on, "latest", None, "todo"), (true, "config"));
        let off = [doc(json!({"todo": false}))];
        assert_eq!(get(&off, "dev", None, "todo"), (false, "config"));
        assert_eq!(get(&off, "latest", Some("todo"), "todo"), (true, "env"));
        assert_eq!(get(&on, "latest", Some(" all , -todo "), "todo"), (false, "env"));
    }

    #[test]
    fn opencode_flags_all_skips_opt_in_only_flags_and_a_minus_key_still_wins() {
        let entries = [doc(json!({"media_blob": false}))];
        assert_eq!(resolve(&entries, "latest", Some("all")), implicit(true, Source::Env));
        assert_eq!(resolve(&[], "latest", Some(" -media_blob , all, ")), with(implicit(true, Source::Env), "media_blob", false, Source::Env));
        assert_eq!(resolve(&entries, "latest", Some("-all")), resolve(&entries, "latest", None));
    }

    #[test]
    fn all_flags_skips_opt_in_only_flags_and_yields_to_opencode_flags_and_per_flag_settings() {
        let entries = [doc(json!({"all_flags": true, "media_blob": false}))];
        let expected = with(with(implicit(true, Source::Config), "media_blob", false, Source::Config), "artifacts", false, Source::Env);
        assert_eq!(resolve(&entries, "latest", Some("-artifacts")), expected);
    }

    #[test]
    fn all_flags_false_is_not_a_kill_switch_and_all_flags_in_opencode_flags_does_nothing() {
        assert_eq!(resolve(&[doc(json!({"all_flags": false}))], "dev", None), implicit(true, Source::Default));
        assert_eq!(resolve(&[], "latest", Some("all_flags")), resolve(&[], "latest", None));
    }

    #[test]
    fn a_global_all_flags_survives_a_project_block_and_a_project_all_flags_false_stops_it() {
        let global = doc(json!({"all_flags": true}));
        assert_eq!(
            resolve(&[global.clone(), doc(json!({"subagent_depth": 2, "todo": false}))], "latest", None),
            with(implicit(true, Source::Config), "todo", false, Source::Config)
        );
        assert_eq!(resolve(&[global, doc(json!({"all_flags": false}))], "latest", None), implicit(false, Source::Default));
    }
}
