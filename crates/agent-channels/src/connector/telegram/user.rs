use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct TgUser {
    pub id: i64,
    /// Telegram's own answer to whether this author is a bot.
    #[serde(default)]
    pub is_bot: Option<bool>,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

impl TgUser {
    pub fn handle(&self) -> String {
        match (&self.username, self.id) {
            (Some(username), _) => format!("@{username}"),
            (None, id) => id.to_string(),
        }
    }

    pub fn display_name(&self) -> String {
        format_name(self.first_name.as_deref(), self.username.as_deref())
            .unwrap_or_else(|| self.id.to_string())
    }

    /// The provider-supplied name, without the numeric fallback used in prose.
    pub fn sender_name(&self) -> Option<String> {
        format_name(
            nonblank(self.first_name.as_deref()),
            nonblank(self.username.as_deref()),
        )
    }
}

fn nonblank(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

fn format_name(name: Option<&str>, username: Option<&str>) -> Option<String> {
    match (name, username) {
        (Some(name), Some(username)) => Some(format!("{name} (@{username})")),
        (Some(name), None) => Some(name.to_owned()),
        (None, Some(username)) => Some(format!("@{username}")),
        (None, None) => None,
    }
}
