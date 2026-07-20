use crate::{AgentServer, AgentServerDelegate, load_proxy_env};
use acp_thread::AgentConnection;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result};
use collections::HashSet;
use fs::Fs;
use gpui::{App, AppContext as _, Entity, Task};
use language_model::{ApiKey, EnvVar};
use project::{
    Project,
    agent_server_store::{AgentId, AllAgentServersSettings},
};
use settings::{AgentConfigOptionValue, SettingsStore, update_settings_file};
use std::{rc::Rc, sync::Arc};
use ui::IconName;

pub const GEMINI_ID: &str = "gemini";
pub const CLAUDE_AGENT_ID: &str = "claude-acp";
pub const CODEX_ID: &str = "codex-acp";
pub const CURSOR_ID: &str = "cursor";

/// A generic agent server implementation for custom user-defined agents
pub struct CustomAgentServer {
    agent_id: AgentId,
}

impl CustomAgentServer {
    pub fn new(agent_id: AgentId) -> Self {
        Self { agent_id }
    }
}

impl AgentServer for CustomAgentServer {
    fn agent_id(&self) -> AgentId {
        self.agent_id.clone()
    }

    fn logo(&self) -> IconName {
        IconName::Terminal
    }

    fn default_mode(&self, cx: &App) -> Option<acp::SessionModeId> {
        let settings = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().0.as_ref())
                .cloned()
        });

        settings
            .as_ref()
            .and_then(|s| s.default_mode().map(acp::SessionModeId::new))
    }

    fn favorite_config_option_value_ids(
        &self,
        config_id: &acp::SessionConfigId,
        cx: &mut App,
    ) -> HashSet<acp::SessionConfigValueId> {
        let settings = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().0.as_ref())
                .cloned()
        });

        settings
            .as_ref()
            .and_then(|s| s.favorite_config_option_values(config_id.0.as_ref()))
            .map(|values| {
                values
                    .iter()
                    .cloned()
                    .map(acp::SessionConfigValueId::new)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn toggle_favorite_config_option_value(
        &self,
        config_id: acp::SessionConfigId,
        value_id: acp::SessionConfigValueId,
        should_be_favorite: bool,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) {
        let agent_id = self.agent_id();
        let config_id = config_id.to_string();
        let value_id = value_id.to_string();

        update_settings_file(fs, cx, move |settings, _cx| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(agent_id.0.to_string())
                .or_insert_with(default_settings_for_agent);

            match settings {
                settings::CustomAgentServerSettings::Custom {
                    favorite_config_option_values,
                    ..
                }
                | settings::CustomAgentServerSettings::Registry {
                    favorite_config_option_values,
                    ..
                } => {
                    let entry = favorite_config_option_values
                        .entry(config_id.clone())
                        .or_insert_with(Vec::new);

                    if should_be_favorite {
                        if !entry.iter().any(|v| v == &value_id) {
                            entry.push(value_id.clone());
                        }
                    } else {
                        entry.retain(|v| v != &value_id);
                        if entry.is_empty() {
                            favorite_config_option_values.remove(&config_id);
                        }
                    }
                }
            }
        });
    }

    fn set_default_mode(&self, mode_id: Option<acp::SessionModeId>, fs: Arc<dyn Fs>, cx: &mut App) {
        let agent_id = self.agent_id();
        update_settings_file(fs, cx, move |settings, _cx| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(agent_id.0.to_string())
                .or_insert_with(default_settings_for_agent);

            match settings {
                settings::CustomAgentServerSettings::Custom { default_mode, .. }
                | settings::CustomAgentServerSettings::Registry { default_mode, .. } => {
                    *default_mode = mode_id.map(|m| m.to_string());
                }
            }
        });
    }

    fn default_config_option(&self, config_id: &str, cx: &App) -> Option<AgentConfigOptionValue> {
        let settings = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().as_ref())
                .cloned()
        });

        settings
            .as_ref()
            .and_then(|s| s.default_config_option(config_id).cloned())
    }

    fn set_default_config_option(
        &self,
        config_id: &str,
        value: Option<AgentConfigOptionValue>,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) {
        let agent_id = self.agent_id();
        let config_id = config_id.to_string();
        update_settings_file(fs, cx, move |settings, _cx| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(agent_id.0.to_string())
                .or_insert_with(default_settings_for_agent);

            match settings {
                settings::CustomAgentServerSettings::Custom {
                    default_config_options,
                    ..
                }
                | settings::CustomAgentServerSettings::Registry {
                    default_config_options,
                    ..
                } => {
                    if let Some(value) = value {
                        default_config_options.insert(config_id.clone(), value);
                    } else {
                        default_config_options.remove(&config_id);
                    }
                }
            }
        });
    }

    fn connect(
        &self,
        delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let agent_id = self.agent_id();
        let default_mode = self.default_mode(cx);
        let is_registry_agent = is_registry_agent(agent_id.clone(), cx);
        let default_config_options = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().as_ref())
                .map(|s| match s {
                    project::agent_server_store::CustomAgentServerSettings::Custom {
                        default_config_options,
                        ..
                    }
                    | project::agent_server_store::CustomAgentServerSettings::Registry {
                        default_config_options,
                        ..
                    } => default_config_options.clone(),
                })
                .unwrap_or_default()
        });

        if is_registry_agent {
            if let Some(registry_store) = project::AgentRegistryStore::try_global(cx) {
                registry_store.update(cx, |store, cx| store.refresh_if_stale(cx));
            }
        }

        let mut extra_env = load_proxy_env(cx);
        if delegate.store.read(cx).no_browser() {
            extra_env.insert("NO_BROWSER".to_owned(), "1".to_owned());
        }
        if is_registry_agent {
            extra_env.extend(registry_agent_env_overrides(agent_id.as_ref(), &|key| {
                std::env::var(key).ok()
            }));
        }
        let store = delegate.store.downgrade();
        cx.spawn(async move |cx| {
            if is_registry_agent && agent_id.as_ref() == GEMINI_ID {
                if let Some(api_key) = cx.update(api_key_for_gemini_cli).await.ok() {
                    extra_env.insert("GEMINI_API_KEY".into(), api_key);
                }
            }
            let command = store
                .update(cx, |store, cx| {
                    let agent = store.get_external_agent(&agent_id).with_context(|| {
                        format!("Custom agent server `{}` is not registered", agent_id)
                    })?;
                    if let Some(new_version_available_tx) = delegate.new_version_available {
                        agent.set_new_version_available_tx(new_version_available_tx);
                    }
                    if let Some(loading_status_tx) = delegate.loading_status {
                        agent.set_loading_status_tx(loading_status_tx);
                    }
                    anyhow::Ok(agent.get_command(vec![], extra_env, &mut cx.to_async()))
                })??
                .await?;
            let connection = crate::acp::connect(
                agent_id,
                project,
                command,
                store.clone(),
                default_mode,
                default_config_options,
                cx,
            )
            .await?;
            Ok(connection)
        })
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn std::any::Any> {
        self
    }
}

fn api_key_for_gemini_cli(cx: &mut App) -> Task<Result<String>> {
    let env_var = EnvVar::new("GEMINI_API_KEY".into()).or(EnvVar::new("GOOGLE_AI_API_KEY".into()));
    if let Some(key) = env_var.value {
        return Task::ready(Ok(key));
    }
    let credentials_provider = zed_credentials_provider::global(cx);
    let api_url = google_ai::API_URL.to_string();
    cx.spawn(async move |cx| {
        Ok(
            ApiKey::load_from_system_keychain(&api_url, credentials_provider.as_ref(), cx)
                .await?
                .key()
                .to_string(),
        )
    })
}

/// Environment overrides Zed injects when launching specific registry-managed
/// agents. Kept as a pure function (with the process environment abstracted
/// behind `process_env`) so the per-agent behavior can be unit tested without
/// spawning a server.
fn registry_agent_env_overrides(
    agent_id: &str,
    process_env: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    match agent_id {
        CLAUDE_AGENT_ID => vec![("ANTHROPIC_API_KEY".to_owned(), String::new())],
        CODEX_ID => ["CODEX_API_KEY", "OPEN_AI_API_KEY"]
            .into_iter()
            .filter_map(|key| Some((key.to_owned(), process_env(key)?)))
            .collect(),
        GEMINI_ID => vec![("SURFACE".to_owned(), "zed".to_owned())],
        _ => Vec::new(),
    }
}

fn is_registry_agent(agent_id: impl Into<AgentId>, cx: &App) -> bool {
    let agent_id = agent_id.into();
    let is_in_registry = project::AgentRegistryStore::try_global(cx)
        .map(|store| store.read(cx).agent(&agent_id).is_some())
        .unwrap_or(false);
    let is_settings_registry = cx.read_global(|settings: &SettingsStore, _| {
        settings
            .get::<AllAgentServersSettings>(None)
            .get(agent_id.as_ref())
            .is_some_and(|s| {
                matches!(
                    s,
                    project::agent_server_store::CustomAgentServerSettings::Registry { .. }
                )
            })
    });
    is_in_registry || is_settings_registry
}

fn default_settings_for_agent() -> settings::CustomAgentServerSettings {
    settings::CustomAgentServerSettings::Registry {
        default_mode: None,
        env: Default::default(),
        default_config_options: Default::default(),
        favorite_config_option_values: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use collections::HashMap;
    use gpui::TestAppContext;
    use project::agent_registry_store::{
        AgentRegistryStore, RegistryAgent, RegistryAgentMetadata, RegistryNpxAgent,
    };
    use settings::Settings as _;
    use ui::SharedString;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    fn init_registry_with_agents(cx: &mut TestAppContext, agent_ids: &[&str]) {
        let agents: Vec<RegistryAgent> = agent_ids
            .iter()
            .map(|id| {
                let id = SharedString::from(id.to_string());
                RegistryAgent::Npx(RegistryNpxAgent {
                    metadata: RegistryAgentMetadata {
                        id: AgentId::new(id.clone()),
                        name: id.clone(),
                        description: SharedString::from(""),
                        version: SharedString::from("1.0.0"),
                        repository: None,
                        website: None,
                        icon_path: None,
                    },
                    package: id,
                    args: Vec::new(),
                    env: HashMap::default(),
                })
            })
            .collect();
        cx.update(|cx| {
            AgentRegistryStore::init_test_global(cx, agents);
        });
    }

    fn set_agent_server_settings(
        cx: &mut TestAppContext,
        entries: Vec<(&str, settings::CustomAgentServerSettings)>,
    ) {
        cx.update(|cx| {
            AllAgentServersSettings::override_global(
                project::agent_server_store::AllAgentServersSettings(
                    entries
                        .into_iter()
                        .map(|(name, settings)| (name.to_string(), settings.into()))
                        .collect(),
                ),
                cx,
            );
        });
    }

    #[test]
    fn test_registry_agent_env_overrides_claude() {
        let overrides = registry_agent_env_overrides(CLAUDE_AGENT_ID, &|_| None);
        assert_eq!(
            overrides,
            vec![("ANTHROPIC_API_KEY".to_owned(), String::new())],
            "claude-acp should get a blanked ANTHROPIC_API_KEY so login state is used"
        );
    }

    #[test]
    fn test_registry_agent_env_overrides_codex() {
        let process_env = |key: &str| match key {
            "CODEX_API_KEY" => Some("codex-key".to_owned()),
            "OPEN_AI_API_KEY" => Some("openai-key".to_owned()),
            _ => None,
        };
        let overrides = registry_agent_env_overrides(CODEX_ID, &process_env);
        assert_eq!(
            overrides,
            vec![
                ("CODEX_API_KEY".to_owned(), "codex-key".to_owned()),
                ("OPEN_AI_API_KEY".to_owned(), "openai-key".to_owned()),
            ],
            "codex-acp should forward API keys from the process environment"
        );

        let overrides = registry_agent_env_overrides(CODEX_ID, &|_| None);
        assert_eq!(
            overrides,
            Vec::new(),
            "codex-acp should not inject keys that are absent from the process environment"
        );
    }

    #[test]
    fn test_registry_agent_env_overrides_gemini_and_unknown() {
        assert_eq!(
            registry_agent_env_overrides(GEMINI_ID, &|_| None),
            vec![("SURFACE".to_owned(), "zed".to_owned())]
        );
        assert_eq!(
            registry_agent_env_overrides("some-other-agent", &|_| Some("value".to_owned())),
            Vec::new(),
            "unknown agents should get no environment overrides"
        );
    }

    #[gpui::test]
    async fn test_custom_agent_settings_round_trip_to_resolved_command(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/root", serde_json::json!({})).await;
        let project = project::Project::test(fs, [std::path::Path::new("/root")], cx).await;

        set_agent_server_settings(
            cx,
            vec![(
                "my-made-up-agent",
                settings::CustomAgentServerSettings::Custom {
                    path: "/bin/my-agent".into(),
                    args: vec!["--acp".to_owned()],
                    env: HashMap::from_iter([("FOO".to_owned(), "bar".to_owned())]),
                    default_mode: None,
                    default_config_options: HashMap::default(),
                    favorite_config_option_values: HashMap::default(),
                },
            )],
        );
        cx.run_until_parked();

        let store = project.read_with(cx, |project, _| project.agent_server_store().clone());
        let command = store
            .update(cx, |store, cx| {
                let agent = store
                    .get_external_agent(&AgentId::new("my-made-up-agent"))
                    .expect("custom agent from settings should be registered in the store");
                agent.get_command(
                    vec!["--extra-arg".to_owned()],
                    HashMap::from_iter([("EXTRA".to_owned(), "1".to_owned())]),
                    &mut cx.to_async(),
                )
            })
            .await
            .expect("resolving the custom agent command should succeed");

        assert_eq!(command.path, std::path::PathBuf::from("/bin/my-agent"));
        assert_eq!(
            command.args,
            vec!["--acp".to_owned(), "--extra-arg".to_owned()]
        );
        let env: HashMap<String, String> = command
            .env
            .expect("resolved command should carry an environment")
            .into_iter()
            .collect();
        assert_eq!(
            env.get("FOO").map(String::as_str),
            Some("bar"),
            "settings env should survive the round trip"
        );
        assert_eq!(
            env.get("EXTRA").map(String::as_str),
            Some("1"),
            "connect-time extra env should be merged into the resolved command"
        );
    }

    #[gpui::test]
    fn test_unknown_agent_is_not_registry(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            assert!(!is_registry_agent("my-custom-agent", cx));
        });
    }

    #[gpui::test]
    fn test_agent_in_registry_store_is_registry(cx: &mut TestAppContext) {
        init_test(cx);
        init_registry_with_agents(cx, &["some-new-registry-agent"]);
        cx.update(|cx| {
            assert!(is_registry_agent("some-new-registry-agent", cx));
            assert!(!is_registry_agent("not-in-registry", cx));
        });
    }

    #[gpui::test]
    fn test_agent_with_registry_settings_type_is_registry(cx: &mut TestAppContext) {
        init_test(cx);
        set_agent_server_settings(
            cx,
            vec![(
                "agent-from-settings",
                settings::CustomAgentServerSettings::Registry {
                    env: HashMap::default(),
                    default_mode: None,
                    default_config_options: HashMap::default(),
                    favorite_config_option_values: HashMap::default(),
                },
            )],
        );
        cx.update(|cx| {
            assert!(is_registry_agent("agent-from-settings", cx));
        });
    }
}
