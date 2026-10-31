//! One retrieval, budgeting and dependency boundary for conversational knowledge.
pub mod assembly;
pub mod compaction;
pub mod continuity;
pub mod policy;
pub mod summary;

#[cfg(test)]
mod tests;

use super::{learning, memory};
use crate::{
    config::AiSettings,
    host::Host,
    store::Store,
    types::{ContextGuard, Scene, Scope, now},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

const SCHEMA: &str = "CREATE TABLE ai_context_reply_dependencies(reply_key TEXT PRIMARY KEY, dependencies TEXT NOT NULL);
CREATE TABLE ai_context_summary_dependencies(conversation_id TEXT PRIMARY KEY, epoch INTEGER NOT NULL, dependencies TEXT NOT NULL);";

const POLICY_SCHEMA: &str = "CREATE TABLE ai_context_policy(
    id INTEGER PRIMARY KEY CHECK(id=1),
    memory_read INTEGER NOT NULL CHECK(memory_read IN (0,1)),
    paused INTEGER NOT NULL CHECK(paused IN (0,1)));";

pub async fn init(store: &Store) -> Result<()> {
    continuity::init(store).await?;
    store.migrate("ai_context_dependencies", 1, SCHEMA).await?;
    store
        .migrate("ai_context_dependencies", 2, POLICY_SCHEMA)
        .await?;
    store.migrate("ai_context_dependencies", 3,
        "CREATE TABLE ai_context_audience(id INTEGER PRIMARY KEY CHECK(id=1),policy TEXT NOT NULL);").await?;
    compaction::init(store).await
}

/// Global defaults live outside the conversation row. Persist their relevant
/// policy so application restarts cannot silently reuse a now-forbidden epoch.
pub async fn configure_policy(store: &Store, defaults: &AiSettings) -> Result<()> {
    let defaults = defaults.clone();
    store
        .call(move |db| configure_policy_in_transaction(db, &defaults))
        .await
}

fn configure_policy_in_transaction(db: &mut Connection, defaults: &AiSettings) -> Result<()> {
    let tx = db.transaction()?;
    let previous: Option<(bool, bool)> = tx
        .query_row(
            "SELECT memory_read,paused FROM ai_context_policy WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let old = previous.unwrap_or((true, false));
    let current = (defaults.memory_read, defaults.paused);
    if previous == Some(current) {
        tx.commit()?;
        return Ok(());
    }
    let conversations = tx
        .prepare("SELECT id,settings FROM ai_conversations ORDER BY id")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, settings) in conversations {
        let old_effective = effective_read_policy(old, &settings)?;
        let effective = effective_read_policy(current, &settings)?;
        if old_effective != effective || (previous.is_none() && (!effective.0 || effective.1)) {
            continuity::invalidate_conversation_replies(&tx, &id)?;
            // Do not carry old raw messages into the new epoch. Re-enabling a
            // default later must not revive a source disabled by this change.
            tx.execute("UPDATE ai_conversations SET epoch=epoch+1,summary='',compacted_until=0,memory_processed_until=0,memory_revision=memory_revision+1 WHERE id=?1", [&id])?;
        }
    }
    tx.execute(
        "INSERT INTO ai_context_policy(id,memory_read,paused) VALUES(1,?1,?2)
        ON CONFLICT(id) DO UPDATE SET memory_read=excluded.memory_read,paused=excluded.paused",
        params![current.0, current.1],
    )?;
    tx.commit()?;
    Ok(())
}

fn effective_read_policy(defaults: (bool, bool), settings: &str) -> Result<(bool, bool)> {
    let settings: serde_json::Value = serde_json::from_str(settings)?;
    ensure!(settings.is_object(), "conversation policy is invalid");
    let field = |name: &str, default: bool| -> Result<bool> {
        settings.get(name).map_or(Ok(default), |value| {
            value.as_bool().context("conversation policy is invalid")
        })
    };
    Ok((
        field("memory_read", defaults.0)?,
        field("paused", defaults.1)?,
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MemoryDependency {
    id: i64,
    version: i64,
}

/// Only selected materials enter this guard. Once used in a model call they remain
/// dependencies for the entire turn, including tool confirmation and sending.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContextDependencyGuard {
    #[serde(default)]
    continuity: continuity::ContinuityGuard,
    #[serde(default)]
    memories: Vec<MemoryDependency>,
    #[serde(default)]
    learning: Vec<learning::LearningCheck>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    personal: Vec<super::personal::Dependency>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    knowledge: Vec<super::knowledge::Dependency>,
}

impl From<learning::LearningCheck> for ContextDependencyGuard {
    fn from(check: learning::LearningCheck) -> Self {
        Self {
            learning: vec![check],
            ..Self::default()
        }
    }
}

impl ContextDependencyGuard {
    pub(crate) fn source_keys(&self) -> Vec<String> {
        self.continuity.source_keys()
    }
    pub fn allows_audience(
        &self,
        db: &Connection,
        policy: crate::config::CrossScenePolicy,
        scope: &Scope,
        actor: i64,
    ) -> Result<bool> {
        for check in &self.personal {
            if !check.allows_audience(db, policy, scope, actor)? {
                return Ok(false);
            }
        }
        for item in &self.knowledge {
            if !item.allows_audience(db, policy, scope, actor)? {
                return Ok(false);
            }
        }
        let mut keys = self.continuity.source_keys();
        for check in &self.learning {
            keys.extend(check.source_keys(db)?);
        }
        for memory in &self.memories {
            let origin: String = db.query_row("SELECT c.scope FROM ai_memories m JOIN ai_conversations c ON c.id=m.conversation_id WHERE m.id=?1", [memory.id], |r| r.get(0))?;
            if !policy::allows_scope_key(policy, &origin, scope, actor) {
                return Ok(false);
            }
            keys.extend(
                db.prepare("SELECT source_key FROM ai_memory_provenance WHERE memory_id=?1")?
                    .query_map([memory.id], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        policy::allows_sources(db, policy, &keys, scope, actor)
    }

    pub fn validate(&self, db: &Connection) -> Result<()> {
        ensure!(
            serde_json::to_vec(self)?.len() <= 262_144,
            "上下文来源依赖超过保存上限"
        );
        self.continuity.validate(db)?;
        for check in &self.personal {
            check.validate(db)?;
        }
        for memory in &self.memories {
            let valid: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM ai_memories WHERE id=?1 AND version=?2 AND active=1 AND (expires_at IS NULL OR expires_at>?3))",
                params![memory.id, memory.version, now()], |r| r.get(0))?;
            ensure!(valid, "上下文中的个人知识已变化或失效");
        }
        for learning in &self.learning {
            learning.validate(db)?;
        }
        for item in &self.knowledge {
            item.validate(db)?;
        }
        Ok(())
    }

    pub fn extend(&mut self, other: &Self) {
        for item in &other.knowledge {
            if !self.knowledge.contains(item) {
                self.knowledge.push(item.clone());
            }
        }
        for check in &other.personal {
            if !self.personal.contains(check) {
                self.personal.push(check.clone());
            }
        }
        self.continuity.extend(&other.continuity);
        for memory in &other.memories {
            if !self
                .memories
                .iter()
                .any(|m| m.id == memory.id && m.version == memory.version)
            {
                self.memories.push(memory.clone());
            }
        }
        // Retain differently selected note sets from all model rounds.
        for check in &other.learning {
            let encoded = serde_json::to_string(check).ok();
            if !self
                .learning
                .iter()
                .any(|prior| serde_json::to_string(prior).ok() == encoded)
            {
                self.learning.push(check.clone());
            }
        }
    }

    /// Stored guards contain flattened dependencies, never nested guard bodies.
    pub fn inherit_replies(&mut self, db: &Connection, keys: &[String]) -> Result<()> {
        if !dependency_tables_exist(db)? {
            return Ok(());
        }
        for key in keys {
            let encoded: Option<String> = db
                .query_row(
                    "SELECT dependencies FROM ai_context_reply_dependencies WHERE reply_key=?1",
                    [key],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(encoded) = encoded {
                let guard: Self = serde_json::from_str(&encoded)?;
                guard.validate(db)?;
                self.extend(&guard);
            }
        }
        Ok(())
    }

    pub fn include_sources(&mut self, db: &Connection, keys: &[String]) -> Result<()> {
        for key in keys {
            self.continuity.add_source_key(key.clone());
        }
        self.inherit_replies(db, keys)
    }

    /// Capture the exact allowed facts supplied to a background consolidation.
    pub fn include_memories(&mut self, db: &Connection, ids: &[i64]) -> Result<()> {
        if !dependency_tables_exist(db)? {
            return Ok(());
        }
        for id in ids {
            let current = memory::get_memory(db, *id)?.context("个人知识已删除")?;
            let version: i64 = db.query_row("SELECT version FROM ai_memories WHERE id=?1 AND active=1 AND (expires_at IS NULL OR expires_at>?2)",
                params![id,now()], |r| r.get(0))?;
            if !self
                .memories
                .iter()
                .any(|m| m.id == *id && m.version == version)
            {
                self.memories.push(MemoryDependency { id: *id, version });
            }
            let mut sources = db
                .prepare("SELECT source_key FROM ai_memory_provenance WHERE memory_id=?1")?
                .query_map([id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            sources.extend(current.evidence.iter().map(|e| e.source_key.clone()));
            self.include_sources(db, &sources)?;
        }
        self.validate(db)
    }

    pub(crate) fn register_reply(&self, db: &Connection, position: i64, key: &str) -> Result<()> {
        // This runs after a confirmed network send. A changed dependency makes the
        // sent text ineligible for reuse; it must not undo its real delivery ledger.
        self.validate(db)?;
        continuity::register_reply(db, position, key, &self.continuity)?;
        db.execute("INSERT OR REPLACE INTO ai_context_reply_dependencies(reply_key,dependencies) VALUES(?1,?2)",
            params![key, serde_json::to_string(self)?])?;
        // Only material that reached a confirmed reply counts as used.
        let memories: Vec<i64> = self.memories.iter().map(|m| m.id).collect();
        let knowledge: Vec<i64> = self.knowledge.iter().map(|k| k.id).collect();
        super::knowledge::stats::record_recall(db, &memories, &knowledge)?;
        Ok(())
    }
}

fn dependency_tables_exist(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='ai_context_reply_dependencies')", [], |r| r.get(0))?)
}

pub fn reply_is_valid(db: &Connection, key: &str) -> Result<bool> {
    let mut guard = ContextDependencyGuard::default();
    Ok(guard
        .include_sources(db, &[key.to_owned()])
        .and_then(|()| guard.validate(db))
        .is_ok())
}

pub fn reply_allows_audience(
    db: &Connection,
    key: &str,
    policy: crate::config::CrossScenePolicy,
    scope: &Scope,
    actor: i64,
) -> Result<bool> {
    let mut guard = ContextDependencyGuard::default();
    if guard
        .include_sources(db, &[key.to_owned()])
        .and_then(|()| guard.validate(db))
        .is_err()
    {
        return Ok(false);
    }
    guard.allows_audience(db, policy, scope, actor)
}

pub fn summary_guard(db: &Connection, guard: &ContextGuard) -> Result<ContextDependencyGuard> {
    if !dependency_tables_exist(db)? {
        return Ok(ContextDependencyGuard::default());
    }
    let encoded: Option<String> = db
        .query_row(
            "SELECT dependencies FROM ai_context_summary_dependencies WHERE conversation_id=?1",
            [&guard.conversation_id],
            |r| r.get(0),
        )
        .optional()?;
    // A settings change may advance the conversation epoch while retaining its
    // summary. Keep its original dependencies; each snapshot revalidates itself.
    let dependencies: ContextDependencyGuard = encoded
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default();
    dependencies.validate(db)?;
    Ok(dependencies)
}

pub fn capture_summary_dependencies(
    db: &Connection,
    batch: &memory::MemoryBatch,
) -> Result<ContextDependencyGuard> {
    if !batch.summary_enabled || !dependency_tables_exist(db)? {
        return Ok(ContextDependencyGuard::default());
    }
    let mut guard = if batch.previous_summary.is_empty() {
        ContextDependencyGuard::default()
    } else {
        summary_guard(db, &batch.guard)?
    };
    let keys = batch
        .sources
        .iter()
        .filter(|s| s.summary_eligible && s.context_visible)
        .map(|s| s.key.clone())
        .collect::<Vec<_>>();
    guard.include_sources(db, &keys)?;
    guard.validate(db)?;
    Ok(guard)
}

pub fn record_summary(db: &Connection, batch: &memory::MemoryBatch) -> Result<()> {
    if !batch.summary_enabled || !dependency_tables_exist(db)? {
        return Ok(());
    }
    batch.summary_dependencies.validate(db)?;
    db.execute("INSERT OR REPLACE INTO ai_context_summary_dependencies(conversation_id,epoch,dependencies) VALUES(?1,?2,?3)",
        params![batch.guard.conversation_id,batch.guard.epoch,serde_json::to_string(&batch.summary_dependencies)?])?;
    Ok(())
}

pub fn maintain(db: &Connection) -> Result<()> {
    if !dependency_tables_exist(db)? {
        return Ok(());
    }
    db.execute_batch("DELETE FROM ai_recent_turns WHERE NOT EXISTS(SELECT 1 FROM messages WHERE key=request_key) OR NOT EXISTS(SELECT 1 FROM messages WHERE key=reply_key);
        DELETE FROM ai_context_sources WHERE NOT EXISTS(SELECT 1 FROM messages WHERE key=source_key);
        DELETE FROM ai_context_relations WHERE NOT EXISTS(SELECT 1 FROM messages WHERE key=source_key);
        DELETE FROM ai_context_reply_dependencies WHERE NOT EXISTS(SELECT 1 FROM messages WHERE key=reply_key);
        DELETE FROM ai_context_dependencies WHERE NOT EXISTS(SELECT 1 FROM messages WHERE key=derived_key);
        DELETE FROM ai_context_summary_dependencies WHERE NOT EXISTS(SELECT 1 FROM ai_conversations WHERE id=conversation_id AND summary!='');")?;
    Ok(())
}

pub struct ContextRequest<'a> {
    pub guard: &'a ContextGuard,
    pub scope: &'a Scope,
    pub source_key: &'a str,
    pub actor: i64,
    pub position: i64,
    pub received_at: i64,
    pub current: &'a str,
    pub settings: &'a AiSettings,
    pub(crate) once_query: Option<&'a super::semantic::OnceQueryPermit>,
    /// Multi-author automatic participation cannot be attributed to the last
    /// speaker as a request to resume their personal conversations.
    pub allow_continuity: bool,
    /// Members the current message mentions or quotes, for same-group recall.
    pub referenced: &'a [i64],
}

enum KnowledgeDependency {
    Memory(MemoryDependency),
    Item(super::knowledge::Dependency),
}

struct Knowledge {
    text: String,
    dependency: KnowledgeDependency,
    sources: Vec<String>,
}

pub struct PreparedContext {
    personal: Option<super::personal::PersonalContext>,
    memories: Vec<Knowledge>,
    pub learning: Option<learning::LearningContext>,
    pub continuity: Vec<continuity::ContinuityDocument>,
}

impl PreparedContext {
    pub fn memory_texts(&self) -> Vec<String> {
        self.personal
            .iter()
            .map(|p| p.text.clone())
            .chain(self.memories.iter().map(|m| m.text.clone()))
            .collect()
    }
    pub fn learning_texts(&self) -> Vec<String> {
        self.learning
            .as_ref()
            .map(|l| l.texts.clone())
            .unwrap_or_default()
    }
    pub fn continuity_texts(&self) -> Vec<String> {
        self.continuity.iter().map(|c| c.text.clone()).collect()
    }

    pub fn selected_guard(
        &self,
        db: &Connection,
        result: &assembly::AssemblyResult,
    ) -> Result<ContextDependencyGuard> {
        let mut guard = ContextDependencyGuard::default();
        let personal_count =
            usize::from(self.personal.is_some() && result.context.memory_count > 0);
        if personal_count > 0 {
            guard
                .personal
                .push(self.personal.as_ref().unwrap().dependency.clone());
        }
        for memory in self
            .memories
            .iter()
            .take(result.context.memory_count - personal_count)
        {
            match &memory.dependency {
                KnowledgeDependency::Memory(m) => guard.memories.push(m.clone()),
                KnowledgeDependency::Item(k) => guard.knowledge.push(k.clone()),
            }
            for key in &memory.sources {
                guard.continuity.add_source_key(key.clone());
            }
        }
        if let Some(learning) = &self.learning {
            let check = learning.check.selected(&result.learning_indices);
            for key in check.source_keys(db)? {
                guard.continuity.add_source_key(key);
            }
            guard.learning.push(check);
        }
        for index in &result.continuity_indices {
            if let Some(document) = self.continuity.get(*index) {
                guard.continuity.extend(&document.guard);
            }
        }
        let keys = guard.continuity.source_keys();
        guard.inherit_replies(db, &keys)?;
        guard.validate(db)?;
        Ok(guard)
    }
}

/// Same-group public facts about referenced members, bounded per member.
fn group_public(
    db: &Connection,
    guard: &ContextGuard,
    audience: &Scope,
    members: &[i64],
    query: &str,
) -> Result<Vec<Knowledge>> {
    let target = audience.key();
    let mut result = Vec::new();
    for member in members {
        // Filter by scope before limiting: the member's facts from other chats
        // must not crowd out the ones stated here.
        let mut taken = 0;
        for candidate in memory::retrieve(db, guard, *member, query, 16, 4000)? {
            if taken >= 2 {
                break;
            }
            let (version, origin, active, expires): (i64, String, bool, Option<i64>) = db.query_row(
                "SELECT m.version,c.scope,m.active,m.expires_at FROM ai_memories m JOIN ai_conversations c ON c.id=m.conversation_id WHERE m.id=?1",
                [candidate.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            if origin != target
                || !active
                || expires.is_some_and(|value| value <= now())
                || candidate.origin == "manual"
            {
                continue;
            }
            let mut sources = db
                .prepare("SELECT source_key FROM ai_memory_provenance WHERE memory_id=?1")?
                .query_map([candidate.id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for evidence in &candidate.evidence {
                if !sources.contains(&evidence.source_key) {
                    sources.push(evidence.source_key.clone());
                }
            }
            // Every source must be a message in this very group.
            if !policy::allows_sources(
                db,
                crate::config::CrossScenePolicy::SameScope,
                &sources,
                audience,
                *member,
            )? {
                continue;
            }
            taken += 1;
            result.push(Knowledge {
                text: format!(
                    "群成员 {} 在本群公开说过（仅限本群资料，不是当前提问者的信息）/ {:?}：{}",
                    member, candidate.kind, candidate.text
                ),
                dependency: KnowledgeDependency::Memory(MemoryDependency {
                    id: candidate.id,
                    version,
                }),
                sources,
            });
        }
    }
    Ok(result)
}

pub async fn prepare(
    host: &Host,
    request: ContextRequest<'_>,
    cancel: &CancellationToken,
) -> Result<PreparedContext> {
    let memories = if request.settings.memory_read {
        super::semantic::retrieve_for_turn(
            host,
            request.guard,
            request.actor,
            request.source_key,
            request.current,
            host.config.plugins.ai.memory_recall_limit,
            host.config.plugins.ai.memory_recall_budget,
            cancel,
            request.once_query,
        )
        .await?
    } else {
        vec![]
    };
    let guard = request.guard.clone();
    let audience = request.scope.clone();
    let actor = request.actor;
    // A multi-author automatic batch is not a personal request by its last speaker.
    let cross_scene = if request.allow_continuity {
        host.config.plugins.ai.context.cross_scene
    } else {
        crate::config::CrossScenePolicy::SameScope
    };
    let memories = host.store.call(move |db| {
        crate::host::check_guard(db, &guard)?;
        let mut result = Vec::new();
        for candidate in memories {
            let current = memory::get_memory(db, candidate.id)?.context("个人知识已删除")?;
            // A concurrent edit cannot be paired with the old recalled text.
            ensure!(current.text == candidate.text && current.subject_id == candidate.subject_id, "个人知识在召回期间发生变化");
            let (version, scope, active, expires): (i64,String,bool,Option<i64>) = db.query_row(
                "SELECT m.version,c.scope,m.active,m.expires_at FROM ai_memories m JOIN ai_conversations c ON c.id=m.conversation_id WHERE m.id=?1",
                [candidate.id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            ensure!(active && expires.is_none_or(|value| value > now()), "个人知识已停用或过期");
            let mut sources = db.prepare("SELECT source_key FROM ai_memory_provenance WHERE memory_id=?1")?
                .query_map([candidate.id], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for evidence in &current.evidence { if !sources.contains(&evidence.source_key) { sources.push(evidence.source_key.clone()); } }
            if !policy::allows_scope_key(cross_scene, &scope, &audience, actor)
                || !policy::allows_sources(db, cross_scene, &sources, &audience, actor)? { continue; }
            let mut inherited = ContextDependencyGuard::default();
            inherited.include_sources(db, &sources)?;
            if !inherited.allows_audience(db, cross_scene, &audience, actor)? { continue; }
            let origin = match current.origin.as_str() { "manual" => "管理员补充（不代表本人自述）", "auto" => "原始消息提取", _ => "历史记录" };
            result.push(Knowledge { text: format!("记忆 #{} / 关于用户 {} / {:?} / {} / 记录来源范围 {}（群限定事实勿泛化）：{}", candidate.id,current.subject_id,current.kind,origin,scope,current.text),
                dependency: KnowledgeDependency::Memory(MemoryDependency {id:candidate.id,version}), sources });
        }
        Ok(result)
    }).await?;
    let mut memories = memories;
    let knowledge_config = host.config.plugins.ai.knowledge.clone();
    let readable = request.settings.memory_read && !request.settings.paused;
    // Facts a mentioned or quoted member stated in this same group. Only for
    // shared group conversations; a person's private or other-group facts
    // never qualify, and the current speaker still cannot become that member.
    if readable
        && knowledge_config.enabled
        && knowledge_config.group_public_recall
        && request.allow_continuity
        && request.scope.scene == Scene::Group
        && request.settings.mode == "group"
    {
        let mut members: Vec<i64> = request
            .referenced
            .iter()
            .copied()
            .filter(|m| *m > 0 && *m != actor && *m != request.scope.bot_id)
            .collect();
        members.sort_unstable();
        members.dedup();
        members.truncate(2);
        if !members.is_empty() {
            let guard = request.guard.clone();
            let audience = request.scope.clone();
            let query = request.current.to_owned();
            let public = host
                .store
                .call(move |db| group_public(db, &guard, &audience, &members, &query))
                .await?;
            memories.extend(public);
        }
    }
    if readable && knowledge_config.enabled && knowledge_config.injection_budget > 0 {
        let guard = request.guard.clone();
        let audience = request.scope.clone();
        let query = request.current.to_owned();
        let budget = knowledge_config
            .injection_budget
            .min(request.settings.input_budget / 4);
        let reference = request.received_at;
        let personal = request.allow_continuity;
        let items = host
            .store
            .call(move |db| {
                crate::host::check_guard(db, &guard)?;
                let domain = memory::scope::resolve(db, &guard.conversation_id)?;
                super::knowledge::recall(
                    db,
                    &super::knowledge::RecallRequest {
                        scope: &audience,
                        actor,
                        policy: cross_scene,
                        domain: &domain,
                        query: &query,
                        budget,
                        reference,
                        personal,
                    },
                )
            })
            .await?;
        memories.extend(items.into_iter().map(|item| Knowledge {
            text: item.text,
            dependency: KnowledgeDependency::Item(item.dependency),
            sources: item.sources,
        }));
    }
    let mut learning = if request.settings.memory_read
        && !request.settings.paused
        && host.config.plugins.ai.learning.enabled
        && request.scope.scene == Scene::Group
    {
        Some(
            learning::recall(
                host,
                request.guard,
                request.scope,
                request.source_key,
                request.actor,
                request.current,
                host.config.plugins.ai.learning.injection_budget,
                cancel,
            )
            .await?,
        )
    } else {
        None
    };
    if let Some(mut notes) = learning.take() {
        let audience = request.scope.clone();
        learning = Some(
            host.store
                .call(move |db| {
                    let mut indices = Vec::new();
                    for index in 0..notes.texts.len() {
                        let check = notes.check.selected(&[index]);
                        let mut inherited = ContextDependencyGuard::from(check.clone());
                        inherited.include_sources(db, &check.source_keys(db)?)?;
                        if inherited.allows_audience(db, cross_scene, &audience, actor)? {
                            indices.push(index);
                        }
                    }
                    notes.texts = indices.iter().map(|i| notes.texts[*i].clone()).collect();
                    notes.check = notes.check.selected(&indices);
                    Ok(notes)
                })
                .await?,
        );
    }
    let continuity = if request.settings.memory_read
        && request.allow_continuity
        && !request.settings.paused
        && host.config.plugins.ai.context.enabled
    {
        let req = continuity::RecallRequest {
            guard: request.guard.clone(),
            scope: request.scope.clone(),
            actor_id: request.actor,
            request_position: request.position,
            received_at: request.received_at,
            read_group_background: host.config.plugins.ai.read_group_background,
        };
        let mut config = host.config.plugins.ai.context.clone();
        config.recall_budget = config.recall_budget.min(request.settings.input_budget / 2);
        config.neighbor_budget = config.neighbor_budget.min(config.recall_budget);
        let defaults = host.config.plugins.ai.defaults.clone();
        host.store
            .call(move |db| continuity::recall(db, &req, &config, &defaults))
            .await?
    } else {
        vec![]
    };
    let personal =
        if request.settings.memory_read && !request.settings.paused && request.allow_continuity {
            let scope = request.scope.clone();
            let config = host.config.plugins.ai.personal.clone();
            let cutoff = Some((request.position, request.received_at));
            host.store
                .call(move |db| {
                    super::personal::recall(db, &scope, actor, &config, cross_scene, cutoff)
                })
                .await?
        } else {
            None
        };
    Ok(PreparedContext {
        personal,
        memories,
        learning,
        continuity,
    })
}
