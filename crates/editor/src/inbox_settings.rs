use settings::{RegisterSetting, Settings, SettingsContent};

#[derive(Clone, RegisterSetting)]
pub(crate) struct InboxSettings {
    pub enabled: bool,
    pub show_status_button: bool,
    pub agent_dir: Option<String>,
}

impl Settings for InboxSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let inbox = content.pi_inbox.as_ref();
        Self {
            enabled: inbox.and_then(|inbox| inbox.enabled).unwrap_or(true),
            show_status_button: inbox
                .and_then(|inbox| inbox.show_status_button)
                .unwrap_or(true),
            agent_dir: inbox.and_then(|inbox| inbox.agent_dir.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_settings_use_defaults() {
        let settings = InboxSettings::from_settings(&SettingsContent::default());
        assert!(settings.enabled);
        assert!(settings.show_status_button);
        assert!(settings.agent_dir.is_none());
    }

    #[test]
    fn explicit_settings_are_preserved() {
        let content = SettingsContent {
            pi_inbox: Some(settings::PiInboxSettingsContent {
                enabled: Some(false),
                show_status_button: Some(false),
                agent_dir: Some("~/custom-agent".into()),
            }),
            ..Default::default()
        };
        let settings = InboxSettings::from_settings(&content);
        assert!(!settings.enabled);
        assert!(!settings.show_status_button);
        assert_eq!(settings.agent_dir.as_deref(), Some("~/custom-agent"));
    }

    #[test]
    fn user_settings_deserialize() -> anyhow::Result<()> {
        let content: SettingsContent = serde_json::from_str(
            r#"{"pi_inbox":{"enabled":false,"show_status_button":false,"agent_dir":"~/custom-agent"}}"#,
        )?;
        let settings = InboxSettings::from_settings(&content);
        assert!(!settings.enabled);
        assert!(!settings.show_status_button);
        assert_eq!(settings.agent_dir.as_deref(), Some("~/custom-agent"));
        Ok(())
    }
}
