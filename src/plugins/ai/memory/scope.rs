//! Long-term memory domains are independent of conversation history and epochs.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

pub(crate) const MIGRATION: &str = r#"
CREATE TABLE ai_memory_settings (
 id INTEGER PRIMARY KEY CHECK(id=1),
 global_shared INTEGER NOT NULL CHECK(global_shared IN (0,1))
);
INSERT INTO ai_memory_settings(id,global_shared) VALUES(1,1);
ALTER TABLE ai_conversations ADD COLUMN memory_scope TEXT NOT NULL DEFAULT '';
CREATE INDEX ai_conversation_memory_scope ON ai_conversations(memory_scope,id);
CREATE VIEW ai_memory_scope_values AS
 WITH parts AS (
  SELECT id,substr(scope,1,instr(scope,':')-1) AS bot,
   substr(scope,instr(scope,':')+1) AS tail FROM ai_conversations
 ), parsed AS (
  SELECT id,bot,substr(tail,1,instr(tail,':')-1) AS scene,
   substr(tail,instr(tail,':')+1) AS peer FROM parts
 )
 SELECT id, CASE WHEN (SELECT global_shared FROM ai_memory_settings WHERE id=1)=1
   AND bot!='' AND bot NOT GLOB '*[^0-9]*' AND CAST(bot AS INTEGER)>0
   AND scene IN ('group','friend')
   AND peer!='' AND peer NOT GLOB '*[^0-9]*' AND CAST(peer AS INTEGER)>0
  THEN 'bot:' || bot
  ELSE 'conversation:' || id END AS memory_scope
 FROM parsed;
UPDATE ai_conversations SET memory_scope=(SELECT v.memory_scope FROM ai_memory_scope_values v WHERE v.id=ai_conversations.id);
CREATE TRIGGER ai_memory_conversation_insert AFTER INSERT ON ai_conversations BEGIN
 UPDATE ai_conversations SET memory_scope=(SELECT memory_scope FROM ai_memory_scope_values WHERE id=NEW.id) WHERE id=NEW.id;
END;
CREATE TRIGGER ai_memory_conversation_scope AFTER UPDATE OF scope ON ai_conversations BEGIN
 UPDATE ai_conversations SET memory_scope=(SELECT memory_scope FROM ai_memory_scope_values WHERE id=NEW.id),memory_revision=memory_revision+1 WHERE id=NEW.id;
END;
CREATE TRIGGER ai_memory_revision_insert AFTER INSERT ON ai_memories BEGIN
 UPDATE ai_conversations SET memory_revision=memory_revision+1 WHERE memory_scope=(SELECT memory_scope FROM ai_conversations WHERE id=NEW.conversation_id);
END;
CREATE TRIGGER ai_memory_revision_update AFTER UPDATE ON ai_memories BEGIN
 UPDATE ai_conversations SET memory_revision=memory_revision+1 WHERE memory_scope IN (SELECT memory_scope FROM ai_conversations WHERE id IN (OLD.conversation_id,NEW.conversation_id));
END;
CREATE TRIGGER ai_memory_revision_delete AFTER DELETE ON ai_memories BEGIN
 UPDATE ai_conversations SET memory_revision=memory_revision+1 WHERE memory_scope=(SELECT memory_scope FROM ai_conversations WHERE id=OLD.conversation_id);
END;
"#;

/// The offline rebuild CLI cannot use Store::open, which also recovers the outbox.
pub fn ensure_schema(db: &Connection) -> Result<()> {
    let tx = db.unchecked_transaction()?;
    let checksum = format!("{:x}", Sha256::digest(MIGRATION.as_bytes()));
    let prior: Option<String> = tx
        .query_row(
            "SELECT checksum FROM schema_migrations WHERE plugin_id='ai_memory' AND version=4",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(prior) = prior {
        ensure!(prior == checksum, "ai_memory migration 4 checksum changed");
    } else {
        tx.execute_batch(MIGRATION)?;
        tx.execute(
            "INSERT INTO schema_migrations(plugin_id,version,checksum) VALUES('ai_memory',4,?1)",
            [checksum],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Use with `conversation_id IN (SELECT id FROM ai_conversations WHERE memory_scope=?N)`.
pub fn resolve(db: &Connection, conversation_id: &str) -> Result<String> {
    let domain: String = db
        .query_row(
            "SELECT memory_scope FROM ai_conversations WHERE id=?1",
            [conversation_id],
            |row| row.get(0),
        )
        .context("memory conversation is unavailable")?;
    ensure!(!domain.is_empty(), "memory domain is uninitialized");
    Ok(domain)
}

/// Apply the configured mode atomically with all affected conversation snapshots.
pub fn configure(db: &Connection, enabled: bool) -> Result<()> {
    let changed = db.execute(
        "UPDATE ai_memory_settings SET global_shared=?1 WHERE id=1 AND global_shared!=?1",
        [enabled],
    )?;
    if changed != 0 {
        db.execute("UPDATE ai_conversations SET memory_scope=(SELECT v.memory_scope FROM ai_memory_scope_values v WHERE v.id=ai_conversations.id),epoch=epoch+1,summary='',compacted_until=0,memory_processed_until=0,memory_revision=memory_revision+1", [])?;
    }
    Ok(())
}

/// Shared capacity is per QQ subject; isolated mode preserves the conversation cap.
pub fn count(db: &Connection, conversation_id: &str, subject_id: i64) -> Result<usize> {
    let domain = resolve(db, conversation_id)?;
    let count: i64 = db.query_row(
        "SELECT count(*) FROM ai_memories WHERE conversation_id IN (SELECT id FROM ai_conversations WHERE memory_scope=?1) AND (?2=0 OR subject_id=?3)",
        params![domain, domain.starts_with("bot:"), subject_id],
        |row| row.get(0),
    )?;
    Ok(usize::try_from(count)?)
}

/// Forgetting clears every same-bot history, including history from a previous mode.
pub fn invalidate(db: &Connection, conversation_id: &str) -> Result<()> {
    let source: String = db.query_row(
        "SELECT scope FROM ai_conversations WHERE id=?1",
        [conversation_id],
        |row| row.get(0),
    )?;
    let bot = source
        .split_once(':')
        .map(|(bot, _)| bot)
        .filter(|bot| bot.parse::<i64>().is_ok_and(|id| id > 0));
    db.execute(
        "UPDATE ai_conversations SET epoch=epoch+1,summary='',compacted_until=0,memory_processed_until=0,memory_revision=memory_revision+1 WHERE id=?1 OR (?2 IS NOT NULL AND substr(scope,1,instr(scope,':')-1)=?2)",
        params![conversation_id, bot],
    )?;
    Ok(())
}
