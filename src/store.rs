use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

type Work = Box<dyn FnOnce(&mut Connection) + Send>;
enum Job {
    Work(Work),
    Stop(oneshot::Sender<()>),
}

/// A single thread owns SQLite. No connection or transaction crosses an await.
#[derive(Clone)]
pub struct Store {
    sender: mpsc::Sender<Job>,
    scope_gates: Arc<Mutex<HashMap<String, Weak<tokio::sync::RwLock<()>>>>>,
}
impl Store {
    pub async fn open(path: impl AsRef<Path>, capacity: usize) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let (sender, mut receiver) = mpsc::channel(capacity);
        let (ready, started) = oneshot::channel();
        std::thread::Builder::new().name("qkbot-sqlite".into()).spawn(move || {
            let result = (|| -> Result<Connection> {
                let conn = Connection::open(path)?;
                conn.busy_timeout(Duration::from_secs(5))?;
                conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA cache_size=-4096; PRAGMA temp_store=MEMORY;")?;
                Ok(conn)
            })();
            let mut conn = match result {
                Ok(conn) => { let _ = ready.send(Ok(())); conn },
                Err(error) => { let _ = ready.send(Err(error)); return; }
            };
            while let Some(job) = receiver.blocking_recv() {
                match job {
                    Job::Work(work) => {
                        // A faulty trusted plugin must not take down the shared DB worker.
                        // Transaction guards unwind before recovery; callers whose callback
                        // panicked receive a closed-reply error instead of hanging forever.
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&mut conn))).is_err() {
                            tracing::error!("database callback panicked; worker remains available");
                        }
                    }
                    Job::Stop(done) => {
                        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
                        drop(conn);
                        let _ = done.send(());
                        return;
                    }
                }
            }
        })?;
        started
            .await
            .context("database worker stopped at startup")??;
        let store = Self {
            sender,
            scope_gates: Arc::new(Mutex::new(HashMap::new())),
        };
        store.call(|c| { c.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations(plugin_id TEXT NOT NULL,version INTEGER NOT NULL,checksum TEXT NOT NULL,PRIMARY KEY(plugin_id,version));")?; Ok(()) }).await?;
        store.migrate("host", 1, HOST_SCHEMA).await?;
        store.migrate("host", 2, MESSAGE_DISPLAY_NAMES).await?;
        store
            .migrate("catalog", 1, crate::catalog::MIGRATION)
            .await?;
        store
            .migrate("scope_access", 1, crate::access::MIGRATION)
            .await?;
        store.call(|c| {
            c.execute("UPDATE outbox SET state='unknown',detail='process stopped after submission' WHERE state='submitted'", [])?;
            c.execute("UPDATE outbox SET state='failed',detail='process stopped before submission' WHERE state='prepared'", [])?;
            Ok(())
        }).await?;
        Ok(store)
    }

    pub async fn call<F, R>(&self, work: F) -> Result<R>
    where
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.sender
            .try_send(Job::Work(Box::new(move |conn| {
                let _ = tx.send(work(conn));
            })))
            .map_err(|_| anyhow!("database queue is full or closed"))?;
        rx.await.context("database worker closed")?
    }

    pub(crate) fn scope_gate(&self, key: &str) -> Result<Arc<tokio::sync::RwLock<()>>> {
        let mut gates = self
            .scope_gates
            .lock()
            .map_err(|_| anyhow!("scope gate unavailable"))?;
        if let Some(gate) = gates.get(key).and_then(Weak::upgrade) {
            return Ok(gate);
        }
        if gates.len() > 4096 {
            gates.retain(|_, gate| gate.strong_count() > 0);
        }
        let gate = Arc::new(tokio::sync::RwLock::new(()));
        gates.insert(key.to_owned(), Arc::downgrade(&gate));
        Ok(gate)
    }

    pub async fn migrate(&self, plugin: &str, version: i64, sql: &str) -> Result<()> {
        let plugin = plugin.to_owned();
        let sql = sql.to_owned();
        self.call(move |c| {
            use rusqlite::OptionalExtension;
            let checksum = format!("{:x}", Sha256::digest(sql.as_bytes()));
            let tx = c.transaction()?;
            let prior: Option<String> = tx
                .query_row(
                    "SELECT checksum FROM schema_migrations WHERE plugin_id=?1 AND version=?2",
                    params![plugin, version],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(prior) = prior {
                if prior != checksum {
                    bail!("migration checksum mismatch: {plugin}/{version}");
                }
                return Ok(());
            }
            tx.execute_batch(&sql)?;
            tx.execute(
                "INSERT INTO schema_migrations VALUES (?1,?2,?3)",
                params![plugin, version, checksum],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Reserved for lifecycle work that must drain past ordinary queue backpressure.
    pub async fn call_wait<F, R>(&self, work: F) -> Result<R>
    where
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(Job::Work(Box::new(move |conn| {
                let _ = tx.send(work(conn));
            })))
            .await
            .map_err(|_| anyhow!("database worker closed"))?;
        rx.await.context("database worker closed")?
    }

    pub async fn activate_plugins(&self, enabled: Vec<String>) -> Result<()> {
        self.call(move |c| {
            let tx = c.transaction()?;
            tx.execute("UPDATE plugin_states SET enabled=0,epoch=epoch+1", [])?;
            for id in enabled {
                tx.execute("INSERT INTO plugin_states(id,epoch,enabled) VALUES(?1,1,1) ON CONFLICT(id) DO UPDATE SET enabled=1", [id])?;
            }
            tx.commit()?; Ok(())
        }).await
    }

    pub async fn epoch(&self, plugin: &str) -> Result<i64> {
        let plugin = plugin.to_owned();
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT epoch FROM plugin_states WHERE id=?1 AND enabled=1",
                [plugin],
                |r| r.get(0),
            )?)
        })
        .await
    }

    pub async fn stop(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(Job::Stop(tx))
            .await
            .map_err(|_| anyhow!("database worker stopped"))?;
        rx.await?;
        Ok(())
    }
}

const HOST_SCHEMA: &str = r#"
CREATE TABLE plugin_states(id TEXT PRIMARY KEY,epoch INTEGER NOT NULL,enabled INTEGER NOT NULL);
CREATE TABLE messages(
 id INTEGER PRIMARY KEY AUTOINCREMENT,key TEXT NOT NULL UNIQUE,scope TEXT NOT NULL,
 conversation_id TEXT NOT NULL,epoch INTEGER NOT NULL,sender_id INTEGER NOT NULL,
 role TEXT NOT NULL,content TEXT NOT NULL,received_at INTEGER NOT NULL,
 inbound_position INTEGER NOT NULL DEFAULT 0,origin_plugin TEXT,learnable INTEGER NOT NULL DEFAULT 0,
 revoked INTEGER NOT NULL DEFAULT 0);
CREATE INDEX messages_context ON messages(conversation_id,epoch,inbound_position,id);
CREATE INDEX messages_age ON messages(received_at);
CREATE TABLE outbox(
 id TEXT PRIMARY KEY,scope TEXT NOT NULL,origin_plugin TEXT NOT NULL,plugin_epoch INTEGER NOT NULL,
 conversation_id TEXT,context_epoch INTEGER,body TEXT NOT NULL,state TEXT NOT NULL,
 detail TEXT NOT NULL DEFAULT '',message_seq INTEGER,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL);
CREATE INDEX outbox_scope ON outbox(scope,created_at);
CREATE TABLE usage_daily(day TEXT NOT NULL,plugin_id TEXT NOT NULL,purpose TEXT NOT NULL,model TEXT NOT NULL,
 requests INTEGER NOT NULL DEFAULT 0,input_tokens INTEGER NOT NULL DEFAULT 0,output_tokens INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(day,plugin_id,purpose,model));
CREATE TABLE ai_conversations(
 id TEXT PRIMARY KEY,scope TEXT NOT NULL,mode TEXT NOT NULL,owner_id INTEGER,epoch INTEGER NOT NULL DEFAULT 1,
 settings TEXT NOT NULL DEFAULT '{}',summary TEXT NOT NULL DEFAULT '',compacted_until INTEGER NOT NULL DEFAULT 0);
CREATE TABLE ai_scope_modes(scope TEXT PRIMARY KEY,mode TEXT NOT NULL,revision INTEGER NOT NULL DEFAULT 0);
"#;

const MESSAGE_DISPLAY_NAMES: &str = r#"
ALTER TABLE messages ADD COLUMN sender_nickname TEXT;
ALTER TABLE messages ADD COLUMN sender_card TEXT;
"#;

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn display_name_migration_preserves_legacy_content_and_is_repeatable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite3");
        let raw = "旧消息原文\n不改写身份标签";
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(HOST_SCHEMA).unwrap();
            db.execute_batch("CREATE TABLE schema_migrations(plugin_id TEXT NOT NULL,version INTEGER NOT NULL,checksum TEXT NOT NULL,PRIMARY KEY(plugin_id,version));").unwrap();
            db.execute(
                "INSERT INTO schema_migrations VALUES('host',1,?1)",
                [format!("{:x}", Sha256::digest(HOST_SCHEMA.as_bytes()))],
            )
            .unwrap();
            db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at) VALUES('1:group:2:1','1:group:2','conversation',1,3,'user',?1,1)", [raw]).unwrap();
        }
        for _ in 0..2 {
            let store = Store::open(&path, 8).await.unwrap();
            let row: (String,Option<String>,Option<String>,i64) = store.call(|db| Ok(db.query_row("SELECT content,sender_nickname,sender_card,sender_id FROM messages WHERE key='1:group:2:1'", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?)).await.unwrap();
            assert_eq!(row, (raw.into(), None, None, 3));
            let count: i64 = store
                .call(|db| {
                    Ok(db.query_row(
                        "SELECT COUNT(*) FROM schema_migrations WHERE plugin_id='host'",
                        [],
                        |row| row.get(0),
                    )?)
                })
                .await
                .unwrap();
            assert_eq!(count, 2);
            store.stop().await.unwrap();
        }
    }

    #[tokio::test]
    async fn callback_panic_rolls_back_its_transaction_and_preserves_worker() {
        let store = Store::open(":memory:", 8).await.unwrap();
        store
            .migrate("panic_test", 1, "CREATE TABLE panic_test(value INTEGER);")
            .await
            .unwrap();
        let failure = store
            .call::<_, ()>(|connection| {
                let tx = connection.transaction()?;
                tx.execute("INSERT INTO panic_test(value) VALUES(1)", [])?;
                panic!("simulated plugin callback failure");
            })
            .await;
        assert!(failure.is_err());
        let count: i64 =
            store
                .call(|connection| {
                    Ok(connection
                        .query_row("SELECT count(*) FROM panic_test", [], |row| row.get(0))?)
                })
                .await
                .unwrap();
        assert_eq!(count, 0);
        store
            .call(|connection| {
                connection.execute("INSERT INTO panic_test(value) VALUES(2)", [])?;
                Ok(())
            })
            .await
            .unwrap();
        store.stop().await.unwrap();
    }
    #[tokio::test]
    async fn migration_checksum_is_enforced() {
        let s = Store::open(":memory:", 8).await.unwrap();
        s.migrate("test", 1, "CREATE TABLE test(id INTEGER);")
            .await
            .unwrap();
        assert!(
            s.migrate("test", 1, "CREATE TABLE test(id TEXT);")
                .await
                .is_err()
        );
        s.stop().await.unwrap();
    }
}
