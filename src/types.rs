use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scene {
    Group,
    Friend,
}

impl Scene {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Friend => "friend",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub bot_id: i64,
    pub scene: Scene,
    pub peer_id: i64,
}
impl Scope {
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.bot_id, self.scene.as_str(), self.peer_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageKey {
    pub scope: Scope,
    pub message_seq: i64,
}
impl MessageKey {
    pub fn key(&self) -> String {
        format!("{}:{}", self.scope.key(), self.message_seq)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incoming {
    pub key: MessageKey,
    pub sender_id: i64,
    pub text: String,
    pub mentions: Vec<i64>,
    pub quote_seq: Option<i64>,
    pub received_at: i64,
}
impl Incoming {
    pub fn is_self(&self) -> bool {
        self.sender_id == self.key.scope.bot_id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextGuard {
    pub conversation_id: String,
    pub epoch: i64,
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
