//! Persistent per-account, per-physical-chat switches. Bot access defaults on;
//! automatic replies default off and require a separate opt-in for each chat.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::OwnedRwLockReadGuard;

use crate::{
    store::Store,
    types::{Scene, Scope},
};

pub(crate) const MIGRATION: &str = r#"
CREATE TABLE scope_access (
 scope TEXT PRIMARY KEY, bot_id INTEGER NOT NULL, scene TEXT NOT NULL,
 peer_id INTEGER NOT NULL, enabled INTEGER NOT NULL, revision INTEGER NOT NULL
);
CREATE INDEX scope_access_disabled ON scope_access(bot_id,enabled,scene,peer_id);
ALTER TABLE messages ADD COLUMN access_revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE outbox ADD COLUMN access_revision INTEGER NOT NULL DEFAULT 0;
"#;

pub(crate) const AUTO_REPLY_MIGRATION: &str = r#"
CREATE TABLE ai_auto_reply_scopes (
 scope TEXT PRIMARY KEY, bot_id INTEGER NOT NULL, scene TEXT NOT NULL,
 peer_id INTEGER NOT NULL, enabled INTEGER NOT NULL
);
CREATE INDEX ai_auto_reply_enabled ON ai_auto_reply_scopes(bot_id,enabled,scene,peer_id);
"#;

#[derive(Debug)]
pub struct AccessDisabled;
impl std::fmt::Display for AccessDisabled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("此会话的 Bot 已关闭或权限已变更")
    }
}
impl std::error::Error for AccessDisabled {}

#[derive(Clone)]
pub struct AccessStore {
    store: Store,
}
impl AccessStore {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
    pub async fn enabled(&self, scope: &Scope) -> Result<bool> {
        Ok(self.allowed_revision(scope).await?.is_some())
    }
    /// Ordinary messages only wake the AI after this physical chat opts in.
    pub async fn auto_reply_enabled(&self, scope: &Scope) -> Result<bool> {
        let key = scope.key();
        self.store
            .call(move |db| auto_reply_enabled(db, &key))
            .await
    }
    pub async fn auto_reply_scopes(&self, bot_id: i64) -> Result<Vec<Scope>> {
        self.store.call(move |db| {
            let mut statement = db.prepare("SELECT scene,peer_id FROM ai_auto_reply_scopes WHERE bot_id=?1 AND enabled=1 ORDER BY scene,peer_id")?;
            statement.query_map([bot_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
                .map(|row| {
                    let (scene, peer_id) = row?;
                    let scene = match scene.as_str() {
                        "group" => Scene::Group,
                        "friend" => Scene::Friend,
                        _ => anyhow::bail!("invalid stored auto-reply scene"),
                    };
                    Ok(Scope { bot_id, scene, peer_id })
                }).collect()
        }).await
    }
    pub async fn set_auto_reply_enabled(&self, scope: Scope, enabled: bool) -> Result<()> {
        ensure!(
            scope.bot_id > 0 && scope.peer_id > 0,
            "invalid account or chat identifier"
        );
        let gate = self.store.scope_gate(&scope.key())?;
        let _exclusive = gate.write_owned().await;
        self.store.call(move |db| {
            let tx = db.transaction()?;
            let key = scope.key();
            if auto_reply_enabled(&tx, &key)? == enabled { return Ok(()); }
            tx.execute("INSERT INTO ai_auto_reply_scopes(scope,bot_id,scene,peer_id,enabled) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(scope) DO UPDATE SET enabled=excluded.enabled", params![key, scope.bot_id, scope.scene.as_str(), scope.peer_id, enabled])?;
            // Both transitions invalidate in-flight AI work, preserving its raw
            // context and summaries in the next epoch for every member mode.
            tx.execute("UPDATE messages SET epoch=epoch+1 WHERE scope=?1 AND EXISTS(SELECT 1 FROM ai_conversations c WHERE c.id=messages.conversation_id AND c.epoch=messages.epoch)", [&key])?;
            tx.execute("UPDATE ai_conversations SET epoch=epoch+1 WHERE scope=?1", [&key])?;
            tx.execute("UPDATE outbox SET state='failed',detail='Auto reply setting changed before submission' WHERE scope=?1 AND state='prepared'", [&key])?;
            let has_tools: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='tool_runs')", [], |row| row.get(0))?;
            if has_tools {
                tx.execute("UPDATE tool_runs SET status='rejected' WHERE scope_key=?1 AND status='pending_confirmation'", [&key])?;
            }
            tx.commit()?;
            Ok(())
        }).await
    }
    pub(crate) async fn allowed_revision(&self, scope: &Scope) -> Result<Option<i64>> {
        let key = scope.key();
        self.store.call(move |db| allowed_revision(db, &key)).await
    }
    pub async fn disabled(&self, bot_id: i64) -> Result<Vec<Scope>> {
        self.store.call(move|db| {
            let mut stmt=db.prepare("SELECT scene,peer_id FROM scope_access WHERE bot_id=?1 AND enabled=0 ORDER BY scene,peer_id")?;
            stmt.query_map([bot_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))?
                .map(|row| {let(scene,peer_id)=row?;let scene=match scene.as_str(){"group"=>Scene::Group,"friend"=>Scene::Friend,_=>anyhow::bail!("invalid stored access scene")};Ok(Scope{bot_id,scene,peer_id})}).collect()
        }).await
    }
    pub async fn set_enabled(&self, scope: Scope, enabled: bool) -> Result<()> {
        ensure!(
            scope.bot_id > 0 && scope.peer_id > 0,
            "invalid account or chat identifier"
        );
        // Once this returns, an earlier admitted send cannot start/finish behind it.
        // Requests already submitted to QQ finish before this switch commits.
        let gate = self.store.scope_gate(&scope.key())?;
        let _exclusive = gate.write_owned().await;
        self.store.call(move|db| {
            let tx=db.transaction()?;let key=scope.key();
            if allowed_revision(&tx,&key)?.is_some()==enabled { return Ok(()); }
            tx.execute("INSERT INTO scope_access(scope,bot_id,scene,peer_id,enabled,revision) VALUES(?1,?2,?3,?4,?5,1) ON CONFLICT(scope) DO UPDATE SET enabled=excluded.enabled,revision=scope_access.revision+1",params![key,scope.bot_id,scope.scene.as_str(),scope.peer_id,enabled])?;
            if !enabled {
                // Preserve the active raw history and summary, but invalidate every old guard.
                tx.execute("UPDATE messages SET epoch=epoch+1 WHERE scope=?1 AND EXISTS(SELECT 1 FROM ai_conversations c WHERE c.id=messages.conversation_id AND c.epoch=messages.epoch)",[&key])?;
                tx.execute("UPDATE ai_conversations SET epoch=epoch+1 WHERE scope=?1",[&key])?;
                tx.execute("UPDATE outbox SET state='failed',detail='Bot disabled before submission' WHERE scope=?1 AND state='prepared'",[&key])?;
                let has_tools:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='tool_runs')",[],|r|r.get(0))?;
                if has_tools { tx.execute("UPDATE tool_runs SET status='rejected' WHERE scope_key=?1 AND status='pending_confirmation'",[&key])?; }
            }
            tx.commit()?;Ok(())
        }).await
    }
    pub(crate) async fn delivery_guard(&self, scope: &Scope) -> Result<OwnedRwLockReadGuard<()>> {
        Ok(self.store.scope_gate(&scope.key())?.read_owned().await)
    }
}

pub(crate) fn auto_reply_enabled(db: &Connection, key: &str) -> Result<bool> {
    Ok(db
        .query_row(
            "SELECT enabled FROM ai_auto_reply_scopes WHERE scope=?1",
            [key],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

pub(crate) fn require_auto_reply(db: &Connection, key: &str) -> Result<()> {
    ensure!(auto_reply_enabled(db, key)?, "此会话的自动接话已关闭");
    Ok(())
}

pub(crate) fn allowed_revision(db: &Connection, key: &str) -> Result<Option<i64>> {
    let row: Option<(bool, i64)> = db
        .query_row(
            "SELECT enabled,revision FROM scope_access WHERE scope=?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match row {
        None => Some(0),
        Some((true, revision)) => Some(revision),
        Some((false, _)) => None,
    })
}
pub(crate) fn require_enabled(db: &Connection, key: &str) -> Result<i64> {
    allowed_revision(db, key)?.ok_or_else(|| AccessDisabled.into())
}
pub(crate) fn check_revision(db: &Connection, key: &str, expected: i64) -> Result<()> {
    if allowed_revision(db, key)? != Some(expected) {
        return Err(AccessDisabled.into());
    }
    Ok(())
}
pub(crate) fn check_conversation(db: &Connection, id: &str) -> Result<()> {
    let scope: String = db
        .query_row(
            "SELECT scope FROM ai_conversations WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .context("conversation not found")?;
    require_enabled(db, &scope)?;
    Ok(())
}
pub(crate) fn message_revision(db: &Connection, key: &str, source_key: &str) -> Result<i64> {
    let revision = db
        .query_row(
            "SELECT access_revision FROM messages WHERE key=?1 AND scope=?2",
            params![source_key, key],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .context("triggering message is no longer available")?;
    check_revision(db, key, revision)?;
    Ok(revision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        plugins::ai::memory::{BatchUpdate, MemoryKind, MemoryStore},
        types::ContextGuard,
    };
    fn scope(bot_id: i64, scene: Scene) -> Scope {
        Scope {
            bot_id,
            scene,
            peer_id: 20,
        }
    }

    #[tokio::test]
    async fn auto_reply_defaults_off_isolates_physical_chats_and_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auto-reply.sqlite3");
        let store = Store::open(&path, 16).await.unwrap();
        let access = AccessStore::new(store.clone());
        let group = scope(1, Scene::Group);
        let friend = scope(1, Scene::Friend);
        let other_account = scope(2, Scene::Group);
        let other_group = Scope {
            peer_id: 21,
            ..group.clone()
        };
        for target in [&group, &friend, &other_account, &other_group] {
            assert!(!access.auto_reply_enabled(target).await.unwrap());
        }
        access
            .set_auto_reply_enabled(group.clone(), true)
            .await
            .unwrap();
        assert!(access.auto_reply_enabled(&group).await.unwrap());
        for target in [&friend, &other_account, &other_group] {
            assert!(!access.auto_reply_enabled(target).await.unwrap());
        }
        access
            .set_auto_reply_enabled(friend.clone(), true)
            .await
            .unwrap();
        access
            .set_auto_reply_enabled(group.clone(), false)
            .await
            .unwrap();
        assert_eq!(
            access.auto_reply_scopes(1).await.unwrap(),
            vec![friend.clone()]
        );
        assert!(access.auto_reply_scopes(2).await.unwrap().is_empty());
        assert!(access.enabled(&group).await.unwrap());
        store.stop().await.unwrap();
        let store = Store::open(&path, 16).await.unwrap();
        let access = AccessStore::new(store.clone());
        assert!(!access.auto_reply_enabled(&group).await.unwrap());
        assert!(access.auto_reply_enabled(&friend).await.unwrap());
        assert!(!access.auto_reply_enabled(&other_account).await.unwrap());
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn auto_reply_transitions_preserve_all_member_contexts_and_invalidate_work() {
        let store = Store::open(":memory:", 16).await.unwrap();
        let access = AccessStore::new(store.clone());
        store.call(|db| {
            db.execute_batch("INSERT INTO ai_conversations(id,scope,mode,summary) VALUES('shared','1:group:20','group','shared summary'),('member','1:group:20','user','member summary'),('other','1:group:21','group','other summary');
                INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at) VALUES('current','1:group:20','shared',1,7,'user','raw history',1),('member','1:group:20','member',1,8,'user','member history',1),('stale','1:group:20','shared',0,7,'user','old epoch',1);
                INSERT INTO outbox(id,scope,origin_plugin,plugin_epoch,conversation_id,context_epoch,body,state,created_at,updated_at) VALUES('send','1:group:20','ai',1,'shared',1,'pending','prepared',1,1);
                CREATE TABLE tool_runs(scope_key TEXT,status TEXT);
                INSERT INTO tool_runs VALUES('1:group:20','pending_confirmation'),('1:group:21','pending_confirmation');")?;
            assert!(require_auto_reply(db, "1:group:20").is_err());
            Ok(())
        }).await.unwrap();
        access
            .set_auto_reply_enabled(scope(1, Scene::Group), true)
            .await
            .unwrap();
        access
            .set_auto_reply_enabled(scope(1, Scene::Group), true)
            .await
            .unwrap();
        store.call(|db| {
            require_auto_reply(db, "1:group:20")?;
            assert_eq!(db.query_row("SELECT count(*) FROM ai_conversations WHERE scope='1:group:20' AND epoch=2 AND summary <> ''", [], |row| row.get::<_, i64>(0))?, 2);
            assert_eq!(db.query_row("SELECT count(*) FROM messages WHERE epoch=2 AND content LIKE '%history'", [], |row| row.get::<_, i64>(0))?, 2);
            assert_eq!(db.query_row("SELECT epoch FROM messages WHERE key='stale'", [], |row| row.get::<_, i64>(0))?, 0);
            assert_eq!(db.query_row("SELECT epoch FROM ai_conversations WHERE id='other'", [], |row| row.get::<_, i64>(0))?, 1);
            assert_eq!(db.query_row("SELECT state FROM outbox WHERE id='send'", [], |row| row.get::<_, String>(0))?, "failed");
            assert_eq!(db.query_row("SELECT status FROM tool_runs WHERE scope_key='1:group:20'", [], |row| row.get::<_, String>(0))?, "rejected");
            assert_eq!(db.query_row("SELECT status FROM tool_runs WHERE scope_key='1:group:21'", [], |row| row.get::<_, String>(0))?, "pending_confirmation");
            Ok(())
        }).await.unwrap();
        access
            .set_auto_reply_enabled(scope(1, Scene::Group), false)
            .await
            .unwrap();
        store.call(|db| {
            assert!(require_auto_reply(db, "1:group:20").is_err());
            assert_eq!(db.query_row("SELECT count(*) FROM ai_conversations WHERE scope='1:group:20' AND epoch=3", [], |row| row.get::<_, i64>(0))?, 2);
            assert_eq!(db.query_row("SELECT count(*) FROM messages WHERE epoch=3", [], |row| row.get::<_, i64>(0))?, 2);
            Ok(())
        }).await.unwrap();
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn enabled_by_default_is_account_and_scene_isolated_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switches.sqlite3");
        let store = Store::open(&path, 16).await.unwrap();
        let access = AccessStore::new(store.clone());
        let group = scope(1, Scene::Group);
        let friend = scope(1, Scene::Friend);
        let other = scope(2, Scene::Group);
        assert!(access.enabled(&group).await.unwrap());
        assert!(access.disabled(1).await.unwrap().is_empty());
        access.set_enabled(group.clone(), false).await.unwrap();
        assert!(!access.enabled(&group).await.unwrap());
        assert!(access.enabled(&friend).await.unwrap());
        assert!(access.enabled(&other).await.unwrap());
        assert_eq!(access.disabled(1).await.unwrap(), vec![group.clone()]);
        assert!(access.disabled(2).await.unwrap().is_empty());
        access.set_enabled(group.clone(), false).await.unwrap();
        assert_eq!(
            store
                .call(
                    |db| Ok(db.query_row("SELECT revision FROM scope_access", [], |r| r
                        .get::<_, i64>(0))?)
                )
                .await
                .unwrap(),
            1
        );
        store.stop().await.unwrap();
        let store = Store::open(&path, 16).await.unwrap();
        let access = AccessStore::new(store.clone());
        assert!(!access.enabled(&group).await.unwrap());
        access.set_enabled(group.clone(), true).await.unwrap();
        assert!(access.enabled(&group).await.unwrap());
        assert_eq!(access.allowed_revision(&group).await.unwrap(), Some(2));
        store.stop().await.unwrap();
        let store = Store::open(&path, 16).await.unwrap();
        let access = AccessStore::new(store.clone());
        assert!(access.enabled(&group).await.unwrap());
        assert_eq!(access.allowed_revision(&group).await.unwrap(), Some(2));
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn disable_preserves_history_summary_and_memories_but_invalidates_old_batches() {
        let store = Store::open(":memory:", 16).await.unwrap();
        let access = AccessStore::new(store.clone());
        let memory = MemoryStore::new(store.clone(), 256);
        memory.init().await.unwrap();
        store.call(|db| {
            db.execute("INSERT INTO ai_conversations(id,scope,mode,summary) VALUES('c','1:group:20','group','此前摘要')",[])?;
            db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,learnable) VALUES('1:group:20:1','1:group:20','c',1,7,'user','喜欢绿茶',1,1,1)",[])?;
            Ok(())
        }).await.unwrap();
        let guard = ContextGuard {
            conversation_id: "c".into(),
            epoch: 1,
        };
        let id = memory
            .remember(
                &guard,
                7,
                "1:group:20:1",
                "喜欢绿茶",
                MemoryKind::Preference,
                None,
            )
            .await
            .unwrap();
        let old = memory.prepare_batch(&guard, 1, true).await.unwrap();
        access
            .set_enabled(scope(1, Scene::Group), false)
            .await
            .unwrap();
        let current = ContextGuard {
            conversation_id: "c".into(),
            epoch: 2,
        };
        assert!(memory.retrieve(&current, 7, "绿茶", 5, 1000).await.is_err());
        assert!(
            memory
                .commit_batch(
                    old.clone(),
                    BatchUpdate {
                        summary: "迟到摘要".into(),
                        changes: vec![]
                    }
                )
                .await
                .is_err()
        );
        access
            .set_enabled(scope(1, Scene::Group), true)
            .await
            .unwrap();
        assert!(
            memory
                .commit_batch(
                    old,
                    BatchUpdate {
                        summary: "迟到摘要".into(),
                        changes: vec![]
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(
            memory.retrieve(&current, 7, "绿茶", 5, 1000).await.unwrap()[0].id,
            id
        );
        store
            .call(|db| {
                assert_eq!(
                    db.query_row(
                        "SELECT epoch,summary FROM ai_conversations WHERE id='c'",
                        [],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                    )?,
                    (2, "此前摘要".into())
                );
                assert_eq!(
                    db.query_row(
                        "SELECT epoch,content,access_revision FROM messages",
                        [],
                        |r| Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?
                        ))
                    )?,
                    (2, "喜欢绿茶".into(), 0)
                );
                assert!(check_revision(db, "1:group:20", 0).is_err());
                Ok(())
            })
            .await
            .unwrap();
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn disabling_waits_for_admitted_delivery_without_blocking_other_chats() {
        let store = Store::open(":memory:", 16).await.unwrap();
        let access = AccessStore::new(store.clone());
        let group = scope(1, Scene::Group);
        let delivery = access.delivery_guard(&group).await.unwrap();
        let closing = access.clone();
        let target = group.clone();
        let mut close = tokio::spawn(async move { closing.set_enabled(target, false).await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), &mut close)
                .await
                .is_err()
        );
        access
            .set_enabled(scope(2, Scene::Group), false)
            .await
            .unwrap();
        assert!(access.enabled(&group).await.unwrap());
        drop(delivery);
        close.await.unwrap().unwrap();
        assert!(!access.enabled(&group).await.unwrap());
        store.stop().await.unwrap();
    }
}
