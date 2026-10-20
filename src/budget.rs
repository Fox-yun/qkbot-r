//! Admission accounting is persisted before HTTP; ambiguous attempts still consume quota.
use crate::{
    config::Limits,
    store::Store,
    types::{ContextGuard, Scope, now},
};
use anyhow::{Result, ensure};
use rusqlite::params;
use serde_json::{Value, json};

#[derive(Clone)]
pub struct Budget {
    store: Store,
    limits: Limits,
}

#[derive(Clone)]
pub struct Admission {
    pub scope: Scope,
    pub actor: i64,
    pub guard: ContextGuard,
    pub plugin_epoch: i64,
}

#[derive(Clone)]
pub enum RequestAdmission {
    Chat(Admission),
    Memory {
        guard: ContextGuard,
        plugin_epoch: i64,
    },
}

impl Budget {
    pub fn new(store: Store, limits: Limits) -> Self {
        Self { store, limits }
    }
    pub async fn init(&self) -> Result<()> {
        self.store.migrate("model_budget", 1, "CREATE TABLE model_admissions(id INTEGER PRIMARY KEY,at INTEGER NOT NULL,purpose TEXT NOT NULL,actor TEXT NOT NULL,conversation_id TEXT NOT NULL); CREATE INDEX model_admissions_time ON model_admissions(at); CREATE INDEX model_admissions_actor ON model_admissions(actor,at); CREATE INDEX model_admissions_conversation ON model_admissions(conversation_id,at);").await
    }
    pub async fn reserve(&self, admission: Option<Admission>) -> Result<()> {
        self.reserve_request(admission.map(RequestAdmission::Chat))
            .await
    }
    pub async fn reserve_request(&self, request: Option<RequestAdmission>) -> Result<()> {
        let limits = self.limits.clone();
        self.store.call(move |db| {
            let tx = db.transaction()?;
            let admission = match request {
                Some(RequestAdmission::Chat(a)) => Some(a),
                Some(RequestAdmission::Memory{guard,plugin_epoch}) => {
                    crate::host::check_guard(&tx,&guard)?;
                    crate::host::check_plugin(&tx,"ai",plugin_epoch)?;
                    None
                },
                None=>None,
            };
            let since = now() - 3600;
            tx.execute("DELETE FROM model_admissions WHERE at<=?1", [since])?;
            let purpose = if admission.is_some() { "foreground" } else { "background" };
            let count: i64 = tx.query_row("SELECT count(*) FROM model_admissions WHERE purpose=?1", [purpose], |r| r.get(0))?;
            let cap = if admission.is_some() { limits.model_global_per_hour } else { limits.memory_requests_per_hour };
            ensure!(count < cap as i64, "模型每小时请求额度已用完");
            let (actor, conversation) = if let Some(a) = admission {
                crate::host::check_guard(&tx, &a.guard)?;
                crate::host::check_plugin(&tx, "ai", a.plugin_epoch)?;
                let actor = format!("{}:{}", a.scope.bot_id, a.actor);
                let users: i64 = tx.query_row("SELECT count(*) FROM model_admissions WHERE purpose='foreground' AND actor=?1", [&actor], |r| r.get(0))?;
                let conversations: i64 = tx.query_row("SELECT count(*) FROM model_admissions WHERE purpose='foreground' AND conversation_id=?1", [&a.guard.conversation_id], |r| r.get(0))?;
                ensure!(users < limits.model_user_per_hour as i64, "当前用户模型请求额度已用完");
                ensure!(conversations < limits.model_conversation_per_hour as i64, "当前会话模型请求额度已用完");
                let sends: i64 = tx.query_row("SELECT count(*) FROM outbox WHERE scope=?1 AND created_at>?2 AND state IN('prepared','submitted','confirmed','unknown')", params![a.scope.key(),since], |r| r.get(0))?;
                ensure!(sends < limits.sends_per_hour as i64, "当前会话发送额度已用完");
                (actor, a.guard.conversation_id)
            } else { (String::new(), String::new()) };
            tx.execute("INSERT INTO model_admissions(at,purpose,actor,conversation_id) VALUES(?1,?2,?3,?4)", params![now(),purpose,actor,conversation])?;
            tx.commit()?;
            Ok(())
        }).await
    }
    pub async fn snapshot(&self) -> Result<Value> {
        let limits = self.limits.clone();
        self.store.call(move |db| {
            let (foreground,background):(i64,i64) = db.query_row("SELECT coalesce(sum(purpose='foreground'),0),coalesce(sum(purpose='background'),0) FROM model_admissions WHERE at>?1", [now()-3600], |r| Ok((r.get(0)?,r.get(1)?)))?;
            let mut query = db.prepare("SELECT day,purpose,model,requests,input_tokens,output_tokens FROM usage_daily WHERE day>=date('now','-6 days') ORDER BY day DESC,purpose,model LIMIT 100")?;
            let usage = query.query_map([], |r| Ok(json!({"day":r.get::<_,String>(0)?,"purpose":r.get::<_,String>(1)?,"model":r.get::<_,String>(2)?,"requests":r.get::<_,i64>(3)?,"input_tokens":r.get::<_,i64>(4)?,"output_tokens":r.get::<_,i64>(5)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(json!({"window_seconds":3600,"limits":{"model_user_per_hour":limits.model_user_per_hour,"model_conversation_per_hour":limits.model_conversation_per_hour,"model_global_per_hour":limits.model_global_per_hour,"memory_requests_per_hour":limits.memory_requests_per_hour},"used":{"foreground":foreground,"background":background},"usage":usage}))
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Scene;
    #[tokio::test]
    async fn admission_is_atomic_durable_and_background_has_a_separate_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota.sqlite");
        let store = Store::open(path.to_str().unwrap(), 32).await.unwrap();
        store.activate_plugins(vec!["ai".into()]).await.unwrap();
        let plugin_epoch = store.epoch("ai").await.unwrap();
        store
            .call(|db| {
                db.execute(
                    "INSERT INTO ai_conversations(id,scope,mode) VALUES('c','1:group:2','group')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let limits = Limits {
            model_user_per_hour: 1,
            model_conversation_per_hour: 2,
            model_global_per_hour: 3,
            memory_requests_per_hour: 1,
            ..Default::default()
        };
        let budget = Budget::new(store.clone(), limits.clone());
        budget.init().await.unwrap();
        let admission = Admission {
            scope: Scope {
                bot_id: 1,
                scene: Scene::Group,
                peer_id: 2,
            },
            actor: 3,
            guard: ContextGuard {
                conversation_id: "c".into(),
                epoch: 1,
            },
            plugin_epoch,
        };
        let (a, b) = tokio::join!(
            budget.reserve(Some(admission.clone())),
            budget.reserve(Some(admission.clone()))
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        budget.reserve(None).await.unwrap();
        assert!(budget.reserve(None).await.is_err());
        let mut second = admission.clone();
        second.actor = 4;
        budget.reserve(Some(second.clone())).await.unwrap();
        second.actor = 5;
        assert!(budget.reserve(Some(second)).await.is_err());
        store.stop().await.unwrap();
        let store = Store::open(path.to_str().unwrap(), 32).await.unwrap();
        let budget = Budget::new(store.clone(), limits);
        assert!(budget.reserve(Some(admission)).await.is_err());
        assert_eq!(budget.snapshot().await.unwrap()["used"]["foreground"], 2);
        store.stop().await.unwrap();
    }
}
