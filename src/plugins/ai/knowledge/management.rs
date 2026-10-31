//! Read-only provenance for the authenticated management page.
use super::*;

fn message_source(db: &Connection, bot: i64, key: &str, quote: &str) -> Result<Option<Value>> {
    let Some((scope_key, sequence)) = key.rsplit_once(':') else {
        return Ok(None);
    };
    let Ok(scope) = continuity::parse_scope(scope_key) else {
        return Ok(None);
    };
    if scope.bot_id != bot || sequence.parse::<i64>().is_err() {
        return Ok(None);
    }
    let message = db
        .query_row(
            "SELECT sender_id,received_at FROM messages WHERE key=?1 AND scope=?2 AND revoked=0",
            params![key, scope_key],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    // Retention can remove a raw message while its saved evidence remains.
    // Keep its exact location and quote, without inventing a sender or time.
    Ok(Some(json!({
        "source_key": key, "scope": scope_key, "message_seq": sequence,
        "sender_id": message.map(|(sender, _)| sender.to_string()),
        "received_at": message.map(|(_, time)| time), "quote": quote,
    })))
}

pub(super) fn source_snapshot(db: &Connection, bot: i64, source: Source) -> Result<Value> {
    let mut result = serde_json::to_value(&source)?;
    let mut messages = Vec::new();
    result["subject_id"] = Value::Null;
    if source.role == "memory" {
        if let Some(id) = source
            .source_key
            .strip_prefix("memory:")
            .and_then(|id| id.parse::<i64>().ok())
        {
            let memory = db.query_row(
                "SELECT m.subject_id,c.scope FROM ai_memories m JOIN ai_conversations c ON c.id=m.conversation_id WHERE m.id=?1 AND m.version=?2",
                params![id, source.memory_version],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            ).optional()?;
            if let Some((subject, scope)) = memory
                && continuity::parse_scope(&scope).is_ok_and(|scope| scope.bot_id == bot)
            {
                result["subject_id"] = json!(subject.to_string());
                let evidence = db.prepare("SELECT source_key,quote FROM ai_memory_provenance WHERE memory_id=?1 ORDER BY source_key,quote")?
                    .query_map([id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (key, quote) in evidence {
                    if let Some(message) = message_source(db, bot, &key, &quote)? {
                        messages.push(message);
                    }
                }
            }
        }
    } else if let Some(message) = message_source(db, bot, &source.source_key, &source.quote)? {
        messages.push(message);
    }
    result["messages"] = json!(messages);
    Ok(result)
}
