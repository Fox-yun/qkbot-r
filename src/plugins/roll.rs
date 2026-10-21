//! Choose one whitespace-separated option, preserving duplicate positions.
use crate::{
    host::Host,
    plugin::{Plugin, PluginSpec},
    types::{Incoming, OutgoingMessage, OutgoingSegment},
};
use anyhow::Result;
use async_trait::async_trait;

pub struct RollPlugin;
const USAGE: &str = "用法：/roll 选项1 选项2（用空格分隔）";

#[async_trait]
impl Plugin for RollPlugin {
    fn spec(&self) -> PluginSpec {
        PluginSpec {
            id: "roll",
            commands: &["/roll"],
            tools: &[],
            dependencies: &["host"],
        }
    }

    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        Ok(self.reply(host, message).await?.text_content())
    }

    async fn reply(&self, host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
        let reply = roll_reply(message, random_u64);
        reply.validate(host.config.limits.max_message_bytes)?;
        Ok(reply)
    }
}

fn options(text: &str) -> Option<Vec<&str>> {
    let mut words = text.split_whitespace();
    (words.next()? == "/roll").then(|| words.collect())
}

fn roll_reply(message: &Incoming, random: impl FnMut() -> u64) -> OutgoingMessage {
    let options = options(&message.text).unwrap_or_default();
    let text = if options.is_empty() {
        USAGE
    } else {
        options[random_index(options.len(), random)]
    };
    OutgoingMessage {
        segments: vec![
            OutgoingSegment::Reply {
                message_seq: message.key.message_seq,
            },
            OutgoingSegment::Text { text: text.into() },
        ],
    }
}

fn random_u64() -> u64 {
    let id = uuid::Uuid::new_v4();
    let bytes = id.as_bytes();
    // UUID v4 reserves bits in bytes 6 and 8. These eight bytes are all random.
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[14], bytes[15],
    ])
}

fn random_index(length: usize, mut random: impl FnMut() -> u64) -> usize {
    assert!(length > 0);
    let bound = length as u64;
    // Discard the incomplete residue cycle so every position has equal weight.
    let threshold = bound.wrapping_neg() % bound;
    loop {
        let value = random();
        if value >= threshold {
            return (value % bound) as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessageKey, NudgeEvent, Scene, Scope};

    fn incoming(text: &str) -> Incoming {
        Incoming {
            key: MessageKey {
                scope: Scope {
                    bot_id: 1,
                    scene: Scene::Group,
                    peer_id: 2,
                },
                message_seq: 123,
            },
            sender_id: 3,
            sender_nickname: None,
            sender_card: None,
            text: text.into(),
            mentions: vec![],
            quote_seq: Some(42),
            received_at: 1,
            raw_message: serde_json::Value::Null,
            notification: None,
        }
    }

    #[test]
    fn parsing_uses_whitespace_and_preserves_order_and_duplicates() {
        assert_eq!(
            options(" \t/roll\n苹果  香蕉\t苹果\u{3000}🍉\r\n"),
            Some(vec!["苹果", "香蕉", "苹果", "🍉"])
        );
        assert_eq!(options("/roll"), Some(vec![]));
        assert_eq!(options("/roll \n\t"), Some(vec![]));
        assert_eq!(options(""), None);
    }

    #[test]
    fn default_claims_require_exact_command_and_ignore_notifications() {
        for text in ["/roll", " /roll a b", "/roll\ta", "/roll\u{3000}a"] {
            assert!(RollPlugin.claims(&incoming(text)), "{text:?}");
        }
        for text in ["/roller a", "/rolla", "/roll/a", "/ROLL a", "a /roll b", ""] {
            assert!(!RollPlugin.claims(&incoming(text)), "{text:?}");
            assert!(options(text).is_none());
        }
        let mut message = incoming("/roll a b");
        message.notification = Some(NudgeEvent {
            event_type: "group_nudge".into(),
            initiator: 3,
            target: 1,
            raw_event: serde_json::Value::Null,
        });
        assert!(!RollPlugin.claims(&message));
    }

    #[test]
    fn single_option_and_usage_reply_to_current_message_not_its_quote() {
        for scene in [Scene::Group, Scene::Friend] {
            for (text, expected) in [("/roll 唯一选项", "唯一选项"), ("/roll \t\n", USAGE)]
            {
                let mut message = incoming(text);
                message.key.scope.scene = scene;
                let reply = roll_reply(&message, || u64::MAX);
                assert_eq!(
                    reply.segments,
                    vec![
                        OutgoingSegment::Reply { message_seq: 123 },
                        OutgoingSegment::Text {
                            text: expected.into(),
                        },
                    ]
                );
                reply.validate(8192).unwrap();
            }
        }
        roll_reply(&incoming("/roll"), || {
            panic!("usage must not draw randomness")
        });
    }

    #[test]
    fn rejection_sampling_excludes_incomplete_cycle_and_stays_in_range() {
        let mut values = [0, 5].into_iter();
        assert_eq!(random_index(3, || values.next().unwrap()), 2);
        assert!(values.next().is_none());

        for length in [1, 2, 3, 7, 128, 4096, usize::MAX] {
            for first in [0, 1, u64::MAX / 2, u64::MAX] {
                let mut values = [first, u64::MAX].into_iter();
                assert!(random_index(length, || values.next().unwrap()) < length);
            }
        }
    }

    #[test]
    fn repeated_options_keep_the_weight_of_each_position() {
        let message = incoming("/roll 苹果 苹果 香蕉");
        let mut positions = [0; 3];
        let mut apples = 0;
        let mut bananas = 0;
        // Three divides this accepted range; every position occurs twenty times.
        for value in 1..=60 {
            positions[random_index(3, || value)] += 1;
            match roll_reply(&message, || value).text_content().as_str() {
                "苹果" => apples += 1,
                "香蕉" => bananas += 1,
                other => panic!("unexpected choice: {other}"),
            }
        }
        assert_eq!(positions, [20, 20, 20]);
        assert_eq!((apples, bananas), (40, 20));
    }

    #[test]
    fn options_follow_host_size_limits_without_a_separate_small_count_limit() {
        let long = "🦀".repeat(2000);
        let message = incoming(&format!("/roll {long}"));
        let reply = roll_reply(&message, || 0);
        assert_eq!(reply.text_content(), long);
        reply.validate(8192).unwrap();
        assert!(reply.validate(7999).is_err());

        let message = incoming(&format!("/roll {}last", "a ".repeat(1000)));
        let reply = roll_reply(&message, || 1000);
        assert_eq!(reply.text_content(), "last");
        reply.validate(8192).unwrap();
    }
}
