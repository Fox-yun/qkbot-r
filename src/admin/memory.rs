//! Trusted web administration over the same SQLite worker as the bot.
//! Receipts and previews contain identifiers/counts, never memory/message bodies.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    config::{AiConfig, AiSettings},
    plugins::ai::{
        memory::{
            MemoryKind, get_memory, lexical_terms, revoke_memories, validate_text, validate_ttl,
        },
        resolve_settings,
    },
    store::Store,
    types::now,
};

const PAGE_SIZE: i64 = 20;
const PREVIEW_TTL: i64 = 300;

#[derive(Debug)]
pub enum AdminError {
    Conflict(&'static str),
    NotFound(&'static str),
}
impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(message) | Self::NotFound(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for AdminError {}
fn require_version(condition: bool, message: &'static str) -> Result<()> {
    if !condition {
        return Err(AdminError::Conflict(message).into());
    }
    Ok(())
}

/// Constructed by authenticated HTTP middleware, never deserialized from a request.
#[derive(Clone, Debug)]
pub struct Actor {
    pub admin_id: String,
    pub session_id: String,
}
impl Actor {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.admin_id.is_empty()
                && self.admin_id.len() <= 128
                && !self.session_id.is_empty()
                && self.session_id.len() <= 128,
            "invalid administrator session"
        );
        Ok(())
    }
}

#[derive(Clone)]
pub struct MemoryAdmin {
    store: Store,
    config: AiConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConversationQuery {
    pub page: Option<usize>,
    pub query: Option<String>,
    pub scope: Option<String>,
    pub mode: Option<String>,
    pub bot_id: Option<i64>,
    pub scene: Option<String>,
    pub peer_id: Option<i64>,
    pub owner_id: Option<i64>,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryQuery {
    pub conversation_id: String,
    pub page: Option<usize>,
    pub keyword: Option<String>,
    pub subject_id: Option<i64>,
    pub active: Option<bool>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationRequest {
    pub request_id: String,
    pub conversation_id: String,
    pub epoch: i64,
    pub memory_revision: i64,
    pub action: Action,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupPersonaRequest {
    pub request_id: String,
    pub bot_id: String,
    pub persona: String,
}
fn yes() -> bool {
    true
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Create {
        subject_id: i64,
        kind: MemoryKind,
        text: String,
        ttl_days: Option<u16>,
        #[serde(default = "yes")]
        active: bool,
    },
    Edit {
        id: i64,
        version: i64,
        kind: MemoryKind,
        text: String,
        ttl_days: Option<u16>,
        #[serde(default = "yes")]
        locked: bool,
        #[serde(default = "yes")]
        active: bool,
    },
    SetActive {
        id: i64,
        version: i64,
        active: bool,
    },
    Settings {
        settings: AiSettings,
    },
    Reset,
    Mode {
        mode: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgetPreviewRequest {
    pub conversation_id: String,
    pub epoch: i64,
    pub memory_revision: i64,
    pub ids: Vec<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgetConfirmRequest {
    pub request_id: String,
    pub preview_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ConversationVersion {
    id: String,
    scope: String,
    epoch: i64,
    revision: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct RecordVersion {
    id: i64,
    version: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ForgetPlan {
    conversation_id: String,
    requested_ids: Vec<i64>,
    records: Vec<RecordVersion>,
    conversations: Vec<ConversationVersion>,
    source_count: i64,
}

impl MemoryAdmin {
    pub fn new(store: Store, config: AiConfig) -> Self {
        Self { store, config }
    }

    pub(crate) async fn group_persona_receipt(
        &self,
        actor: &Actor,
        request: &GroupPersonaRequest,
    ) -> Result<Option<Value>> {
        actor.validate()?;
        validate_request_id(&request.request_id)?;
        let hash = request_hash(request)?;
        let actor = actor.clone();
        let id = request.request_id.clone();
        self.store
            .call(move |db| receipt(db, &actor, &id, Some(&hash)))
            .await
    }

    /// The caller supplies freshly verified membership for the requested account.
    /// A scope seed covers future member-isolated conversations without changing
    /// global defaults or adding another layer to every context resolution.
    pub(crate) async fn set_group_persona(
        &self,
        actor: &Actor,
        request: GroupPersonaRequest,
        group_ids: Vec<i64>,
    ) -> Result<Value> {
        actor.validate()?;
        validate_request_id(&request.request_id)?;
        let bot_id = request.bot_id.parse::<i64>()?;
        ensure!(
            bot_id > 0 && request.bot_id == bot_id.to_string(),
            "invalid bot_id"
        );
        ensure!(
            group_ids.iter().all(|id| *id > 0),
            "invalid group identifier"
        );
        let mut scopes: Vec<_> = group_ids
            .iter()
            .map(|id| format!("{bot_id}:group:{id}"))
            .collect();
        scopes.sort();
        scopes.dedup();
        // Finish already-started sends before changing epochs; new sends then
        // observe the committed generation. Stable ordering avoids lock cycles.
        let mut gates = Vec::with_capacity(scopes.len());
        for scope in &scopes {
            gates.push(self.store.scope_gate(scope)?.write_owned().await);
        }
        let actor = actor.clone();
        let config = self.config.clone();
        self.store.call(move |db| {
            let hash = request_hash(&request)?;
            let tx = db.transaction()?;
            if let Some(result) = receipt(&tx, &actor, &request.request_id, Some(&hash))? {
                return Ok(result);
            }
            ensure!(config.persona(&request.persona).is_some(),
                "当前运行配置中没有该人设；如刚保存了自定义人设，请先应用全局配置");
            let mut affected_conversations = 0;
            for scope in &scopes {
                tx.execute("INSERT INTO ai_group_personas(scope,persona) VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET persona=excluded.persona",
                    params![scope,request.persona])?;
                let rows = {
                    let mut statement = tx.prepare("SELECT id,settings FROM ai_conversations WHERE scope=?1 ORDER BY id")?;
                    statement.query_map([scope], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                for (id, raw) in rows {
                    let mut settings: Value = serde_json::from_str(&raw)?;
                    let object = settings.as_object_mut().context("会话设置格式无效")?;
                    if object.get("persona").and_then(Value::as_str) == Some(&request.persona) {
                        continue;
                    }
                    object.insert("persona".into(), json!(request.persona));
                    let version = conversation_version(&tx, &id)?;
                    tx.execute("UPDATE ai_conversations SET settings=?1 WHERE id=?2", params![settings.to_string(),id])?;
                    bump_context(&tx, &version, true, false)?;
                    affected_conversations += 1;
                }
            }
            let result = json!({"request_id":request.request_id,"status":"completed",
                "bot_id":request.bot_id,"persona":request.persona,
                "affected_groups":scopes.len(),"affected_conversations":affected_conversations,"scopes":scopes});
            save_receipt(&tx,&actor,&request.request_id,&hash,&result)?;
            tx.commit()?;
            Ok(result)
        }).await
    }

    pub async fn conversations(&self, actor: &Actor, query: ConversationQuery) -> Result<Value> {
        actor.validate()?;
        let defaults = self.config.defaults.clone();
        self.store.call(move |db| {
            let offset = page_offset(query.page)?;
            ensure!(query.query.as_ref().is_none_or(|q|q.len()<=512), "query too long");
            // Scope components are metadata, not a text search through message bodies.
            let predicate = "(?1 IS NULL OR instr(c.id,?1)>0) AND (?2 IS NULL OR c.scope=?2) AND (?3 IS NULL OR c.mode=?3) AND (?4 IS NULL OR substr(c.scope,1,instr(c.scope,':')-1)=CAST(?4 AS TEXT)) AND (?5 IS NULL OR substr(c.scope,instr(c.scope,':')+1,instr(substr(c.scope,instr(c.scope,':')+1),':')-1)=?5) AND (?6 IS NULL OR substr(substr(c.scope,instr(c.scope,':')+1),instr(substr(c.scope,instr(c.scope,':')+1),':')+1)=CAST(?6 AS TEXT)) AND (?7 IS NULL OR c.owner_id=?7)";
            let args = params![query.query,query.scope,query.mode,query.bot_id,query.scene,query.peer_id,query.owner_id];
            let total:i64=db.query_row(&format!("SELECT count(*) FROM ai_conversations c WHERE {predicate}"),args,|r|r.get(0))?;
            let mut statement=db.prepare(&format!("SELECT c.id FROM ai_conversations c WHERE {predicate} ORDER BY c.id LIMIT 20 OFFSET ?8"))?;
            let ids=statement.query_map(params![query.query,query.scope,query.mode,query.bot_id,query.scene,query.peer_id,query.owner_id,offset],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let items=ids.iter().map(|id|conversation_value(db,id,&defaults,false)).collect::<Result<Vec<_>>>()?;
            Ok(page_value(items,total,query.page))
        }).await
    }

    pub async fn conversation(&self, actor: &Actor, conversation_id: &str) -> Result<Value> {
        actor.validate()?;
        let id = conversation_id.to_owned();
        let defaults = self.config.defaults.clone();
        self.store
            .call(move |db| conversation_value(db, &id, &defaults, true))
            .await
    }

    pub async fn memories(&self, actor: &Actor, query: MemoryQuery) -> Result<Value> {
        actor.validate()?;
        self.store.call(move|db| {
            let offset=page_offset(query.page)?;
            ensure!(query.keyword.as_ref().is_none_or(|q|q.len()<=512),"keyword too long");
            conversation_version(db,&query.conversation_id)?;
            let predicate="conversation_id=?1 AND (?2 IS NULL OR instr(lower(text),lower(?2))>0) AND (?3 IS NULL OR subject_id=?3) AND (?4 IS NULL OR active=?4)";
            let total:i64=db.query_row(&format!("SELECT count(*) FROM ai_memories WHERE {predicate}"),params![query.conversation_id,query.keyword,query.subject_id,query.active],|r|r.get(0))?;
            let mut stmt=db.prepare(&format!("SELECT {RECORD_COLUMNS} FROM ai_memories WHERE {predicate} ORDER BY updated_at DESC,id DESC LIMIT 20 OFFSET ?5"))?;
            let items=stmt.query_map(params![query.conversation_id,query.keyword,query.subject_id,query.active,offset],record_value)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(page_value(items,total,query.page))
        }).await
    }

    pub async fn detail(
        &self,
        actor: &Actor,
        id: i64,
        evidence_page: Option<usize>,
    ) -> Result<Value> {
        actor.validate()?;
        self.store.call(move|db| {
            let mut item=record(db,id)?;
            let offset=page_offset(evidence_page)?;
            let total:i64=db.query_row("SELECT count(*) FROM ai_memory_provenance WHERE memory_id=?1",[id],|r|r.get(0))?;
            let mut stmt=db.prepare("SELECT source_key,quote FROM ai_memory_provenance WHERE memory_id=?1 ORDER BY source_key,quote LIMIT 20 OFFSET ?2")?;
            let items=stmt.query_map(params![id,offset],|r|Ok(json!({"source_key":r.get::<_,String>(0)?,"quote":r.get::<_,String>(1)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
            item["evidence"]=page_value(items,total,evidence_page);
            Ok(item)
        }).await
    }

    pub async fn source(&self, actor: &Actor, id: i64, source_key: &str) -> Result<Value> {
        actor.validate()?;
        let key = source_key.to_owned();
        self.store.call(move|db| {
            ensure!(key.len()<=512,"source key too long");
            let linked:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM ai_memory_provenance p JOIN ai_memories m ON m.id=p.memory_id WHERE p.memory_id=?1 AND p.source_key=?2)",params![id,key],|r|r.get(0))?;
            ensure!(linked,"source does not belong to this memory");
            let value=db.query_row("SELECT m.content,m.sender_id,m.role,m.received_at,m.conversation_id FROM messages m WHERE m.key=?1 AND m.revoked=0 AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources r WHERE r.source_key=m.key)",[&key],|r|Ok(json!({"available":true,"source_key":key,"text":r.get::<_,String>(0)?,"subject_id":r.get::<_,i64>(1)?,"role":r.get::<_,String>(2)?,"received_at":r.get::<_,i64>(3)?,"conversation_id":r.get::<_,String>(4)?}))).optional()?;
            Ok(value.unwrap_or_else(||json!({"available":false,"source_key":key})))
        }).await
    }

    pub async fn mutate(&self, actor: &Actor, request: MutationRequest) -> Result<Value> {
        actor.validate()?;
        validate_request_id(&request.request_id)?;
        let actor = actor.clone();
        let config = self.config.clone();
        self.store.call(move|db| {
            let hash=request_hash(&request)?;
            let tx=db.transaction()?;
            if let Some(result)=receipt(&tx,&actor,&request.request_id,Some(&hash))? { return Ok(result); }
            let cv=check_version(&tx,&request.conversation_id,request.epoch,request.memory_revision)?;
            let mut ids=Vec::new();
            let scopes=vec![cv.scope.clone()];
            match &request.action {
                Action::Create{subject_id,kind,text,ttl_days,active} => {
                    validate_subject(&tx,&cv.id,*subject_id)?;
                    validate_text(text)?; validate_ttl(*ttl_days)?;
                    let count:i64=tx.query_row("SELECT count(*) FROM ai_memories WHERE conversation_id=?1",[&cv.id],|r|r.get(0))?;
                    ensure!(usize::try_from(count)?<config.max_memories_per_conversation,"memory capacity reached");
                    let expiry=ttl_days.map(|d|now()+i64::from(d)*86400);
                    tx.execute("INSERT INTO ai_memories(conversation_id,subject_id,kind,text,evidence,created_at,updated_at,expires_at,origin,active,locked,version,edited_by) VALUES(?1,?2,?3,?4,'[]',?5,?5,?6,'manual',?7,1,1,?8)",params![cv.id,subject_id,kind.as_str(),text,now(),expiry,active,actor.admin_id])?;
                    let id=tx.last_insert_rowid(); index_memory(&tx,id,text,*active)?;ids.push(id);
                    bump_context(&tx,&cv,true,true)?;
                }
                Action::Edit{id,version,kind,text,ttl_days,locked,active}=> {
                    validate_text(text)?;validate_ttl(*ttl_days)?;
                    let expiry=ttl_days.map(|d|now()+i64::from(d)*86400);
                    require_version(tx.execute("UPDATE ai_memories SET kind=?1,text=?2,expires_at=?3,origin='manual',active=?4,locked=?5,edited_by=?6,version=version+1,updated_at=?7 WHERE id=?8 AND conversation_id=?9 AND version=?10",params![kind.as_str(),text,expiry,active,locked,actor.admin_id,now(),id,cv.id,version])?==1,"memory version conflict")?;
                    index_memory(&tx,*id,text,*active)?;ids.push(*id);
                    bump_context(&tx,&cv,true,true)?;
                }
                Action::SetActive{id,version,active}=> {
                    require_version(tx.execute("UPDATE ai_memories SET active=?1,locked=1,edited_by=?2,version=version+1,updated_at=?3 WHERE id=?4 AND conversation_id=?5 AND version=?6",params![active,actor.admin_id,now(),id,cv.id,version])?==1,"memory version conflict")?;
                    let text:String=tx.query_row("SELECT text FROM ai_memories WHERE id=?1",[id],|r|r.get(0))?;
                    index_memory(&tx,*id,&text,*active)?;ids.push(*id);
                    bump_context(&tx,&cv,true,true)?;
                }
                Action::Settings{settings}=> {
                    ensure!(config.persona(&settings.persona).is_some(),"当前运行配置中没有该人设；如刚保存了自定义人设，请先应用全局配置");
                    config.validate_settings(settings)?;
                    ensure!(settings.allowed_tools.iter().all(|id|config.defaults.allowed_tools.contains(id)),"当前运行配置未允许该工具；如刚修改全局工具列表，请先应用配置");
                    let mode:String=tx.query_row("SELECT mode FROM ai_conversations WHERE id=?1",[&cv.id],|r|r.get(0))?;
                    ensure!(settings.mode==mode,"change conversation mode through the mode operation");
                    let defaults=serde_json::to_value(&config.defaults)?;
                    let mut value=serde_json::to_value(settings)?;
                    let seeded:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ai_group_personas WHERE scope=?1)",[&cv.scope],|r|r.get(0))?;
                    value.as_object_mut().unwrap().retain(|k,v|k!="mode" && ((k=="persona" && seeded) || defaults.get(k)!=Some(v)));
                    tx.execute("UPDATE ai_conversations SET settings=?1 WHERE id=?2",params![value.to_string(),cv.id])?;
                    bump_context(&tx,&cv,true,false)?;
                }
                Action::Reset=>bump_context(&tx,&cv,false,true)?,
                Action::Mode{mode}=> {
                    ensure!(mode=="group" || mode=="user","invalid conversation mode");
                    ensure!(cv.scope.split(':').nth(1)==Some("group"),"friend conversations always use user mode");
                    tx.execute("INSERT INTO ai_scope_modes(scope,mode,revision) VALUES(?1,?2,1) ON CONFLICT(scope) DO UPDATE SET mode=excluded.mode,revision=revision+1",params![cv.scope,mode])?;
                    tx.execute("UPDATE ai_conversations SET epoch=epoch+1,memory_revision=memory_revision+1,summary='',compacted_until=0,memory_processed_until=0 WHERE scope=?1",[&cv.scope])?;
                }
            }
            let after=conversation_version(&tx,&cv.id)?;
            let result=json!({"request_id":request.request_id,"status":"completed","scopes":scopes,"ids":ids,"epoch":after.epoch,"memory_revision":after.revision});
            save_receipt(&tx,&actor,&request.request_id,&hash,&result)?;
            tx.commit()?; Ok(result)
        }).await
    }

    pub async fn preview_forget(
        &self,
        actor: &Actor,
        request: ForgetPreviewRequest,
    ) -> Result<Value> {
        actor.validate()?;
        let actor = actor.clone();
        self.store.call(move|db| {
            let tx=db.transaction()?;
            check_version(&tx,&request.conversation_id,request.epoch,request.memory_revision)?;
            let plan=forget_plan(&tx,&request.conversation_id,&request.ids)?;
            let id=uuid::Uuid::new_v4().to_string(); let expires=now()+PREVIEW_TTL;
            tx.execute("DELETE FROM ai_memory_previews WHERE expires_at<=?1",[now()])?;
            let pending:i64=tx.query_row("SELECT count(*) FROM ai_memory_previews WHERE admin_id=?1 AND session_id=?2",params![actor.admin_id,actor.session_id],|r|r.get(0))?;
            ensure!(pending<32,"too many pending previews");
            tx.execute("INSERT INTO ai_memory_previews(id,admin_id,session_id,snapshot,expires_at) VALUES(?1,?2,?3,?4,?5)",params![id,actor.admin_id,actor.session_id,serde_json::to_string(&plan)?,expires])?;
            let result=json!({"preview_id":id,"expires_at":expires,"ids":plan.records.iter().map(|r|r.id).collect::<Vec<_>>(),"requested_count":plan.requested_ids.len(),"affected_count":plan.records.len(),"source_count":plan.source_count,"scopes":plan.conversations.iter().map(|c|&c.scope).collect::<std::collections::BTreeSet<_>>()});
            tx.commit()?;Ok(result)
        }).await
    }

    pub async fn confirm_forget(
        &self,
        actor: &Actor,
        request: ForgetConfirmRequest,
    ) -> Result<Value> {
        actor.validate()?;
        validate_request_id(&request.request_id)?;
        let actor = actor.clone();
        self.store.call(move|db| {
            let tx=db.transaction()?;let hash=request_hash(&request)?;
            if let Some(result)=receipt(&tx,&actor,&request.request_id,Some(&hash))? { return Ok(result); }
            let (snapshot,expiry):(String,i64)=tx.query_row("SELECT snapshot,expires_at FROM ai_memory_previews WHERE id=?1 AND admin_id=?2 AND session_id=?3",params![request.preview_id,actor.admin_id,actor.session_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or(AdminError::NotFound("preview is unavailable for this session"))?;
            require_version(expiry>now(),"preview expired; review a new preview")?;
            let plan:ForgetPlan=serde_json::from_str(&snapshot)?;
            require_version(forget_plan(&tx,&plan.conversation_id,&plan.requested_ids)?==plan,"preview version conflict; review a new preview")?;
            let targets=plan.requested_ids.iter().map(|id|get_memory(&tx,*id)?.context("memory no longer exists")).collect::<Result<Vec<_>>>()?;
            revoke_memories(&tx,&targets)?;
            for cv in &plan.conversations { bump_context(&tx,cv,false,true)?; }
            let scopes=plan.conversations.iter().map(|c|c.scope.clone()).collect::<std::collections::BTreeSet<_>>();
            let cv=conversation_version(&tx,&plan.conversation_id)?;
            let result=json!({"request_id":request.request_id,"status":"completed","scopes":scopes,"ids":plan.records.iter().map(|r|r.id).collect::<Vec<_>>(),"deleted_count":plan.records.len(),"source_count":plan.source_count,"epoch":cv.epoch,"memory_revision":cv.revision});
            save_receipt(&tx,&actor,&request.request_id,&hash,&result)?;
            tx.execute("DELETE FROM ai_memory_previews WHERE id=?1",[request.preview_id])?;
            tx.commit()?;Ok(result)
        }).await
    }

    pub async fn operation(&self, actor: &Actor, request_id: &str) -> Result<Value> {
        actor.validate()?;
        validate_request_id(request_id)?;
        let actor = actor.clone();
        let id = request_id.to_owned();
        self.store
            .call(move |db| {
                Ok(receipt(db, &actor, &id, None)?
                    .unwrap_or_else(|| json!({"request_id":id,"status":"not_found"})))
            })
            .await
    }
}

const RECORD_COLUMNS: &str = "id,conversation_id,subject_id,kind,text,origin,active,locked,version,edited_by,expires_at,created_at,updated_at";
fn record_value(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id":r.get::<_,i64>(0)?,"conversation_id":r.get::<_,String>(1)?,"subject_id":r.get::<_,i64>(2)?,"kind":r.get::<_,String>(3)?,"text":r.get::<_,String>(4)?,"origin":r.get::<_,String>(5)?,"active":r.get::<_,bool>(6)?,"locked":r.get::<_,bool>(7)?,"version":r.get::<_,i64>(8)?,"edited_by":r.get::<_,Option<String>>(9)?,"expires_at":r.get::<_,Option<i64>>(10)?,"created_at":r.get::<_,i64>(11)?,"updated_at":r.get::<_,i64>(12)?}),
    )
}
fn record(db: &Connection, id: i64) -> Result<Value> {
    db.query_row(
        &format!("SELECT {RECORD_COLUMNS} FROM ai_memories WHERE id=?1"),
        [id],
        record_value,
    )
    .optional()?
    .ok_or_else(|| AdminError::NotFound("memory not found").into())
}
fn page_offset(page: Option<usize>) -> Result<i64> {
    let page = page.unwrap_or(1);
    ensure!(page > 0, "page starts at 1");
    i64::try_from(
        (page - 1)
            .checked_mul(PAGE_SIZE as usize)
            .context("page overflow")?,
    )
    .context("page overflow")
}
fn page_value(items: Vec<Value>, total: i64, page: Option<usize>) -> Value {
    json!({"items":items,"total":total,"page":page.unwrap_or(1),"total_pages":((total+PAGE_SIZE-1)/PAGE_SIZE).max(1)})
}
fn conversation_version(db: &Connection, id: &str) -> Result<ConversationVersion> {
    db.query_row(
        "SELECT id,scope,epoch,memory_revision FROM ai_conversations WHERE id=?1",
        [id],
        |r| {
            Ok(ConversationVersion {
                id: r.get(0)?,
                scope: r.get(1)?,
                epoch: r.get(2)?,
                revision: r.get(3)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| AdminError::NotFound("conversation not found").into())
}
fn check_version(
    db: &Connection,
    id: &str,
    epoch: i64,
    revision: i64,
) -> Result<ConversationVersion> {
    let cv = conversation_version(db, id)?;
    require_version(
        cv.epoch == epoch && cv.revision == revision,
        "conversation version conflict",
    )?;
    Ok(cv)
}
fn conversation_value(
    db: &Connection,
    id: &str,
    defaults: &AiSettings,
    detail: bool,
) -> Result<Value> {
    let mut value=db.query_row("SELECT id,scope,mode,owner_id,epoch,memory_revision,settings FROM ai_conversations WHERE id=?1",[id],|r|Ok(json!({"id":r.get::<_,String>(0)?,"scope":r.get::<_,String>(1)?,"mode":r.get::<_,String>(2)?,"owner_id":r.get::<_,Option<i64>>(3)?,"epoch":r.get::<_,i64>(4)?,"memory_revision":r.get::<_,i64>(5)?,"settings_raw":r.get::<_,String>(6)?}))).optional()?.ok_or(AdminError::NotFound("conversation not found"))?;
    let mut settings = resolve_settings(defaults, value["settings_raw"].as_str().unwrap())?;
    settings.mode = value["mode"].as_str().unwrap().to_owned();
    value.as_object_mut().unwrap().remove("settings_raw");
    value["settings"] = serde_json::to_value(settings)?;
    value["memory_count"] = json!(db.query_row(
        "SELECT count(*) FROM ai_memories WHERE conversation_id=?1",
        [id],
        |r| r.get::<_, i64>(0)
    )?);
    let scope = value["scope"].as_str().unwrap().to_owned();
    let pieces: Vec<_> = scope.split(':').collect();
    if pieces.len() == 3 {
        value["bot_id"] = json!(pieces[0].parse::<i64>().ok());
        value["scene"] = json!(pieces[1]);
        value["peer_id"] = json!(pieces[2].parse::<i64>().ok());
    }
    let (scope_mode, scope_revision) = if pieces.get(1) == Some(&"friend") {
        ("user".to_owned(), 0_i64)
    } else {
        db.query_row(
            "SELECT mode,revision FROM ai_scope_modes WHERE scope=?1",
            [&scope],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?
        .unwrap_or_else(|| (defaults.mode.clone(), 0))
    };
    value["scope_mode"] = json!(scope_mode);
    value["scope_revision"] = json!(scope_revision);
    value["is_current"] = json!(id.starts_with(&format!("{scope}:v{scope_revision}:")));
    if detail {
        value["summary"] = json!(db.query_row(
            "SELECT summary FROM ai_conversations WHERE id=?1",
            [id],
            |r| r.get::<_, String>(0)
        )?);
        value["context_count"]=json!(db.query_row("SELECT count(*) FROM messages WHERE conversation_id=?1 AND epoch=?2 AND revoked=0 AND COALESCE(origin_plugin,'') NOT IN ('control','memory_only') AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources r WHERE r.source_key=messages.key)",params![id,value["epoch"].as_i64()],|r|r.get::<_,i64>(0))?);
    }
    Ok(value)
}
fn validate_subject(db: &Connection, conversation_id: &str, subject: i64) -> Result<()> {
    ensure!(subject > 0, "subject must be a positive QQ identifier");
    let owner: Option<i64> = db.query_row(
        "SELECT owner_id FROM ai_conversations WHERE id=?1",
        [conversation_id],
        |r| r.get(0),
    )?;
    ensure!(
        owner.is_none_or(|id| id == subject),
        "subject is outside this isolated conversation"
    );
    Ok(())
}
fn index_memory(db: &Connection, id: i64, text: &str, active: bool) -> Result<()> {
    db.execute("DELETE FROM ai_memory_search WHERE memory_id=?1", [id])?;
    if active {
        db.execute(
            "INSERT INTO ai_memory_search(memory_id,terms) VALUES(?1,?2)",
            params![id, lexical_terms(text).join(" ")],
        )?;
    }
    Ok(())
}
fn bump_context(
    db: &Connection,
    cv: &ConversationVersion,
    preserve_messages: bool,
    clear_summary: bool,
) -> Result<()> {
    require_version(db.execute("UPDATE ai_conversations SET epoch=epoch+1,memory_revision=memory_revision+1,summary=CASE WHEN ?3 THEN '' ELSE summary END,compacted_until=CASE WHEN ?3 THEN 0 ELSE compacted_until END,memory_processed_until=CASE WHEN ?4 THEN memory_processed_until ELSE 0 END WHERE id=?1 AND epoch=?2 AND memory_revision=?5",params![cv.id,cv.epoch,clear_summary,preserve_messages,cv.revision])?==1,"conversation version conflict")?;
    if preserve_messages {
        db.execute(
            "UPDATE messages SET epoch=epoch+1 WHERE conversation_id=?1 AND epoch=?2",
            params![cv.id, cv.epoch],
        )?;
    }
    Ok(())
}
fn validate_request_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)),
        "invalid request_id"
    );
    Ok(())
}
fn request_hash(value: &impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}
fn receipt(db: &Connection, actor: &Actor, id: &str, hash: Option<&str>) -> Result<Option<Value>> {
    let row:Option<(String,String)>=db.query_row("SELECT request_hash,result FROM ai_memory_operations WHERE admin_id=?1 AND session_id=?2 AND request_id=?3",params![actor.admin_id,actor.session_id,id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    row.map(|(stored, result)| {
        require_version(
            hash.is_none_or(|h| h == stored),
            "request_id was already used for different parameters",
        )?;
        Ok(serde_json::from_str(&result)?)
    })
    .transpose()
}
fn save_receipt(
    db: &Connection,
    actor: &Actor,
    id: &str,
    hash: &str,
    result: &Value,
) -> Result<()> {
    db.execute("INSERT INTO ai_memory_operations(admin_id,session_id,request_id,request_hash,result,created_at) VALUES(?1,?2,?3,?4,?5,?6)",params![actor.admin_id,actor.session_id,id,hash,result.to_string(),now()])?;
    Ok(())
}

// Include legacy evidence snapshots as well as the complete historical provenance.
const PLAN_KEYS: &str = "WITH keys AS MATERIALIZED (SELECT p.source_key FROM ai_memory_provenance p WHERE p.memory_id IN (SELECT value FROM json_each(?1)) UNION SELECT json_extract(e.value,'$.source_key') FROM ai_memories m,json_each(m.evidence) e WHERE m.id IN (SELECT value FROM json_each(?1)))";
fn forget_plan(db: &Connection, conversation_id: &str, ids: &[i64]) -> Result<ForgetPlan> {
    ensure!(
        !ids.is_empty() && ids.len() <= 1000,
        "select between 1 and 1000 records per preview"
    );
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let encoded = serde_json::to_string(&ids)?;
    let count:i64=db.query_row("SELECT count(*) FROM ai_memories WHERE conversation_id=?1 AND id IN (SELECT value FROM json_each(?2))",params![conversation_id,encoded],|r|r.get(0))?;
    require_version(
        count == i64::try_from(ids.len())?,
        "selected memory is missing or outside this conversation",
    )?;
    let mut stmt=db.prepare(&format!("{PLAN_KEYS} SELECT m.id,m.version FROM ai_memories m WHERE m.id IN (SELECT value FROM json_each(?1)) OR EXISTS(SELECT 1 FROM ai_memory_provenance p JOIN keys k ON k.source_key=p.source_key WHERE p.memory_id=m.id) ORDER BY m.id"))?;
    let records = stmt
        .query_map([&encoded], |r| {
            Ok(RecordVersion {
                id: r.get(0)?,
                version: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let affected = serde_json::to_string(&records.iter().map(|r| r.id).collect::<Vec<_>>())?;
    let mut stmt=db.prepare("SELECT DISTINCT conversation_id FROM ai_memories WHERE id IN (SELECT value FROM json_each(?1)) ORDER BY conversation_id")?;
    let conversations = stmt
        .query_map([affected], |r| r.get::<_, String>(0))?
        .map(|row| conversation_version(db, &row?))
        .collect::<Result<Vec<_>>>()?;
    let source_count = db.query_row(
        &format!("{PLAN_KEYS} SELECT count(*) FROM keys"),
        [encoded],
        |r| r.get(0),
    )?;
    Ok(ForgetPlan {
        conversation_id: conversation_id.to_owned(),
        requested_ids: ids,
        records,
        conversations,
        source_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{plugins::ai::memory::MemoryStore, types::ContextGuard};

    async fn fixture() -> (tempfile::TempDir, Store, MemoryAdmin, Actor) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("test.sqlite").to_str().unwrap(), 32)
            .await
            .unwrap();
        MemoryStore::new(store.clone(), 1024).init().await.unwrap();
        store.call(|db| {
            db.execute_batch("INSERT INTO ai_conversations(id,scope,mode,owner_id) VALUES('group','1:group:10','group',NULL),('private','1:group:20','user',7),('other','2:group:10','group',NULL); INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,learnable) VALUES('1:group:10:1','1:group:10','group',1,7,'user','我喜欢红茶',1,1,1);")?; Ok(())
        }).await.unwrap();
        let config = AiConfig {
            max_memories_per_conversation: 1024,
            ..Default::default()
        };
        let service = MemoryAdmin::new(store.clone(), config);
        (
            dir,
            store,
            service,
            Actor {
                admin_id: "administrator".into(),
                session_id: "session-a".into(),
            },
        )
    }
    fn create(id: &str, epoch: i64, revision: i64) -> MutationRequest {
        MutationRequest {
            request_id: id.into(),
            conversation_id: "group".into(),
            epoch,
            memory_revision: revision,
            action: Action::Create {
                subject_id: 7,
                kind: MemoryKind::Preference,
                text: "人工记录正文".into(),
                ttl_days: None,
                active: true,
            },
        }
    }
    fn preview_request(cv: &Value, ids: Vec<i64>) -> ForgetPreviewRequest {
        ForgetPreviewRequest {
            conversation_id: "group".into(),
            epoch: cv["epoch"].as_i64().unwrap(),
            memory_revision: cv["memory_revision"].as_i64().unwrap(),
            ids,
        }
    }
    fn group_persona_request(id: &str, persona: &str) -> GroupPersonaRequest {
        GroupPersonaRequest {
            request_id: id.into(),
            bot_id: "1".into(),
            persona: persona.into(),
        }
    }

    #[tokio::test]
    async fn group_persona_batch_preserves_context_isolation_and_is_idempotent() {
        let (_dir, store, service, actor) = fixture().await;
        let seeded = service
            .mutate(&actor, create("seed-group-memory", 1, 0))
            .await
            .unwrap();
        let memory_id = seeded["ids"][0].as_i64().unwrap();
        store.call(move |db| {
            db.execute_batch("INSERT INTO ai_conversations(id,scope,mode,owner_id) VALUES
                ('friend','1:friend:10','user',10),('left-group','1:group:30','group',NULL);
                UPDATE ai_conversations SET settings='{\"output_budget\":321,\"memory_read\":false}',
                summary='保留摘要',compacted_until=1,memory_processed_until=1 WHERE id='group';
                INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position)
                VALUES('old-reset-message','1:group:10','group',1,7,'user','旧世代不恢复',1,0);")?;
            db.execute("INSERT INTO ai_memory_provenance(memory_id,source_key,quote) VALUES(?1,'1:group:10:1','喜欢红茶')",[memory_id])?;
            Ok(())
        }).await.unwrap();
        let request = group_persona_request("all-groups", "温柔学长");
        let changed = service
            .set_group_persona(&actor, request.clone(), vec![10, 20, 40, 10])
            .await
            .unwrap();
        assert_eq!(changed["affected_groups"], 3);
        assert_eq!(changed["affected_conversations"], 2);
        assert_eq!(
            service.operation(&actor, "all-groups").await.unwrap(),
            changed
        );
        assert_eq!(
            service
                .set_group_persona(&actor, request.clone(), vec![10, 20, 40])
                .await
                .unwrap(),
            changed
        );
        let detail = service.conversation(&actor, "group").await.unwrap();
        assert_eq!(detail["epoch"], 3);
        assert_eq!(detail["settings"]["persona"], "温柔学长");
        assert_eq!(detail["settings"]["output_budget"], 321);
        assert_eq!(detail["settings"]["memory_read"], false);
        assert_eq!(detail["summary"], "保留摘要");
        assert_eq!(detail["context_count"], 1);
        assert_eq!(detail["memory_count"], 1);
        assert_eq!(
            service.conversation(&actor, "private").await.unwrap()["settings"]["persona"],
            "温柔学长"
        );
        for id in ["friend", "other", "left-group"] {
            let detail = service.conversation(&actor, id).await.unwrap();
            assert_eq!(detail["epoch"], 1);
            assert_eq!(
                detail["settings"]["persona"],
                service.config.defaults.persona
            );
        }
        store.call(move |db| {
            assert!(crate::host::check_guard(db,&ContextGuard{conversation_id:"group".into(),epoch:2}).is_err());
            let row:(i64,i64,i64)=db.query_row("SELECT compacted_until,memory_processed_until,memory_revision FROM ai_conversations WHERE id='group'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            assert_eq!(row,(1,1,2));
            let old_epoch:i64=db.query_row("SELECT epoch FROM messages WHERE key='old-reset-message'",[],|r|r.get(0))?;
            assert_eq!(old_epoch,1);
            assert_eq!(db.query_row("SELECT quote FROM ai_memory_provenance WHERE memory_id=?1",[memory_id],|r|r.get::<_,String>(0))?,"喜欢红茶");
            assert_eq!(db.query_row("SELECT count(*) FROM ai_memory_search WHERE memory_id=?1",[memory_id],|r|r.get::<_,i64>(0))?,1);
            assert_eq!(db.query_row("SELECT persona FROM ai_group_personas WHERE scope='1:group:40'",[],|r|r.get::<_,String>(0))?,"温柔学长");
            Ok(())
        }).await.unwrap();
        let mut conflicting = request;
        conflicting.persona = "理性搭档".into();
        assert!(
            service
                .set_group_persona(&actor, conflicting, vec![10])
                .await
                .unwrap_err()
                .is::<AdminError>()
        );
        // Saving a different setting must not erase a batch choice that happens
        // to equal the global default.
        service
            .set_group_persona(
                &actor,
                group_persona_request("default-groups", &service.config.defaults.persona),
                vec![10],
            )
            .await
            .unwrap();
        let detail = service.conversation(&actor, "group").await.unwrap();
        let mut settings: AiSettings = serde_json::from_value(detail["settings"].clone()).unwrap();
        settings.output_budget = 456;
        service
            .mutate(
                &actor,
                MutationRequest {
                    request_id: "save-other-setting".into(),
                    conversation_id: "group".into(),
                    epoch: detail["epoch"].as_i64().unwrap(),
                    memory_revision: detail["memory_revision"].as_i64().unwrap(),
                    action: Action::Settings { settings },
                },
            )
            .await
            .unwrap();
        let raw = store
            .call(|db| {
                Ok(db.query_row(
                    "SELECT settings FROM ai_conversations WHERE id='group'",
                    [],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap()["persona"],
            service.config.defaults.persona
        );
    }

    #[tokio::test]
    async fn group_persona_validation_and_mid_batch_errors_roll_back_every_change() {
        let (_dir, store, service, actor) = fixture().await;
        assert!(
            service
                .set_group_persona(
                    &actor,
                    group_persona_request("unknown-persona", "missing"),
                    vec![10, 20]
                )
                .await
                .is_err()
        );
        store
            .call(|db| {
                db.execute(
                    "UPDATE ai_conversations SET settings='[]' WHERE id='private'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            service
                .set_group_persona(
                    &actor,
                    group_persona_request("rollback-groups", "温柔学长"),
                    vec![10, 20]
                )
                .await
                .is_err()
        );
        store
            .call(|db| {
                assert_eq!(
                    db.query_row("SELECT count(*) FROM ai_group_personas", [], |r| r
                        .get::<_, i64>(0))?,
                    0
                );
                assert_eq!(
                    db.query_row("SELECT count(*) FROM ai_memory_operations", [], |r| r
                        .get::<_, i64>(0))?,
                    0
                );
                assert_eq!(
                    db.query_row(
                        "SELECT settings,epoch FROM ai_conversations WHERE id='group'",
                        [],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                    )?,
                    ("{}".into(), 1)
                );
                assert_eq!(
                    db.query_row(
                        "SELECT epoch FROM messages WHERE key='1:group:10:1'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    1
                );
                Ok(())
            })
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn manual_writes_are_idempotent_locked_and_have_no_fabricated_sources() {
        let (_dir, store, service, actor) = fixture().await;
        let request = create("create-one", 1, 0);
        let result = service.mutate(&actor, request.clone()).await.unwrap();
        assert_eq!(
            service.mutate(&actor, request.clone()).await.unwrap(),
            result
        );
        assert_eq!(
            service.operation(&actor, "create-one").await.unwrap(),
            result
        );
        let id = result["ids"][0].as_i64().unwrap();
        let item = service.detail(&actor, id, None).await.unwrap();
        assert_eq!(item["origin"], "manual");
        assert_eq!(item["locked"], true);
        assert_eq!(item["evidence"]["total"], 0);
        assert!(service.source(&actor, id, "1:group:10:1").await.is_err());
        let mut changed = request;
        changed.action = Action::Create {
            subject_id: 7,
            kind: MemoryKind::Fact,
            text: "不同参数".into(),
            ttl_days: None,
            active: true,
        };
        assert!(matches!(
            service
                .mutate(&actor, changed)
                .await
                .unwrap_err()
                .downcast_ref::<AdminError>(),
            Some(AdminError::Conflict(_))
        ));
        let mut wrong = create("wrong-owner", 1, 0);
        wrong.conversation_id = "private".into();
        wrong.action = Action::Create {
            subject_id: 8,
            kind: MemoryKind::Fact,
            text: "其他人的记忆".into(),
            ttl_days: None,
            active: true,
        };
        assert!(service.mutate(&actor, wrong).await.is_err());
        store
            .call(|db| {
                let audit: String = db.query_row(
                    "SELECT group_concat(result) FROM ai_memory_operations",
                    [],
                    |r| r.get(0),
                )?;
                assert!(!audit.contains("人工记录正文"));
                assert!(!audit.contains("不同参数"));
                assert_eq!(
                    db.query_row(
                        "SELECT epoch FROM messages WHERE key='1:group:10:1'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    2
                );
                Ok(())
            })
            .await
            .unwrap();
        store.stop().await.unwrap();
    }
    #[tokio::test]
    async fn editing_preserves_provenance_and_invalidates_frozen_compaction() {
        let (_dir, store, service, actor) = fixture().await;
        let memory = MemoryStore::new(store.clone(), 1024);
        let guard = ContextGuard {
            conversation_id: "group".into(),
            epoch: 1,
        };
        let batch = memory.prepare_batch(&guard, 1, true).await.unwrap();
        let result = service
            .mutate(&actor, create("create", 1, 0))
            .await
            .unwrap();
        assert!(
            memory
                .commit_batch(
                    batch,
                    crate::plugins::ai::memory::BatchUpdate {
                        summary: "旧摘要".into(),
                        changes: vec![]
                    }
                )
                .await
                .is_err()
        );
        let id = result["ids"][0].as_i64().unwrap();
        let request = MutationRequest {
            request_id: "edit".into(),
            conversation_id: "group".into(),
            epoch: 2,
            memory_revision: 1,
            action: Action::Edit {
                id,
                version: 1,
                kind: MemoryKind::Fact,
                text: "改后的记录".into(),
                ttl_days: None,
                locked: true,
                active: false,
            },
        };
        let result = service.mutate(&actor, request.clone()).await.unwrap();
        assert_eq!(result["epoch"], 3);
        let mut stale = request;
        stale.request_id = "stale".into();
        assert!(matches!(
            service
                .mutate(&actor, stale)
                .await
                .unwrap_err()
                .downcast_ref::<AdminError>(),
            Some(AdminError::Conflict(_))
        ));
        assert!(
            memory
                .retrieve(
                    &ContextGuard {
                        conversation_id: "group".into(),
                        epoch: 3
                    },
                    7,
                    "记录",
                    5,
                    1000
                )
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            service.detail(&actor, id, None).await.unwrap()["version"],
            2
        );
        store.stop().await.unwrap();
    }
    #[tokio::test]
    async fn forgetting_binds_session_versions_and_atomically_revokes_all_related_sources() {
        let (_dir, store, service, actor) = fixture().await;
        store.call(|db| {
            for (id,conversation,text) in [(1,"group","初始红茶"),(2,"group","更新红茶"),(3,"other","派生红茶")] {
                db.execute("INSERT INTO ai_memories(id,conversation_id,subject_id,kind,text,evidence,created_at,updated_at,origin) VALUES(?1,?2,7,'fact',?3,'[]',1,1,'auto')",params![id,conversation,text])?;
                db.execute("INSERT INTO ai_memory_provenance(memory_id,source_key,quote) VALUES(?1,'1:group:10:1','红茶')",[id])?;
                index_memory(db,id,text,true)?;
            }
            db.execute("INSERT INTO ai_memory_provenance(memory_id,source_key,quote) VALUES(2,'1:group:10:2','绿茶')",[])?;
            db.execute("UPDATE ai_conversations SET summary='旧摘要',compacted_until=1,memory_processed_until=1",[])?;
            Ok(())
        }).await.unwrap();
        assert_eq!(
            service.detail(&actor, 2, None).await.unwrap()["evidence"]["total"],
            2
        );
        assert_eq!(
            service.source(&actor, 1, "1:group:10:1").await.unwrap()["text"],
            "我喜欢红茶"
        );
        assert_eq!(
            service.source(&actor, 2, "1:group:10:2").await.unwrap()["available"],
            false
        );
        let cv = service.conversation(&actor, "group").await.unwrap();
        let preview = service
            .preview_forget(&actor, preview_request(&cv, vec![2, 1]))
            .await
            .unwrap();
        assert_eq!(preview["affected_count"], 3);
        assert_eq!(preview["source_count"], 2);
        let confirm = ForgetConfirmRequest {
            request_id: "forget-all".into(),
            preview_id: preview["preview_id"].as_str().unwrap().into(),
        };
        let other = Actor {
            admin_id: actor.admin_id.clone(),
            session_id: "other-session".into(),
        };
        assert!(
            service
                .confirm_forget(&other, confirm.clone())
                .await
                .is_err()
        );
        let result = service
            .confirm_forget(&actor, confirm.clone())
            .await
            .unwrap();
        assert_eq!(
            service.confirm_forget(&actor, confirm).await.unwrap(),
            result
        );
        assert_eq!(result["deleted_count"], 3);
        store.call(|db| {
            for table in ["ai_memories","ai_memory_search","ai_memory_provenance"] { assert_eq!(db.query_row(&format!("SELECT count(*) FROM {table}"),[],|r|r.get::<_,i64>(0))?,0); }
            assert_eq!(db.query_row("SELECT count(*) FROM ai_revoked_sources",[],|r|r.get::<_,i64>(0))?,2);
            assert!(db.query_row("SELECT revoked FROM messages WHERE key='1:group:10:1'",[],|r|r.get::<_,bool>(0))?);
            assert_eq!(db.query_row("SELECT epoch,summary,memory_revision FROM ai_conversations WHERE id='other'",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))?,(2,String::new(),1));
            let snapshot:Option<String>=db.query_row("SELECT snapshot FROM ai_memory_previews LIMIT 1",[],|r|r.get(0)).optional()?;assert!(snapshot.is_none());
            let audit:String=db.query_row("SELECT result FROM ai_memory_operations WHERE request_id='forget-all'",[],|r|r.get(0))?;assert!(!audit.contains("红茶"));assert!(!audit.contains("绿茶"));
            Ok(())
        }).await.unwrap();
        store.stop().await.unwrap();
    }
    #[tokio::test]
    async fn persona_changes_invalidate_old_work_without_erasing_context_or_memory() {
        let (_dir, store, service, actor) = fixture().await;
        service
            .mutate(&actor, create("seed-persona-memory", 1, 0))
            .await
            .unwrap();
        store
            .call(|db| {
                db.execute(
                    "UPDATE ai_conversations SET summary='原有摘要',settings='{\"persona\":\"默认\"}' WHERE id='group'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let old_detail = service.conversation(&actor, "group").await.unwrap();
        assert_eq!(old_detail["settings"]["persona"], "默认");
        assert!(
            service
                .conversations(&actor, ConversationQuery::default())
                .await
                .is_ok()
        );
        let settings = AiSettings {
            persona: "温柔学长".into(),
            ..Default::default()
        };
        let mut change = MutationRequest {
            request_id: "select-persona".into(),
            conversation_id: "group".into(),
            epoch: 2,
            memory_revision: 1,
            action: Action::Settings {
                settings: settings.clone(),
            },
        };
        let result = service.mutate(&actor, change.clone()).await.unwrap();
        assert_eq!(result["epoch"], 3);
        let detail = service.conversation(&actor, "group").await.unwrap();
        assert_eq!(detail["settings"]["persona"], "温柔学长");
        assert_eq!(detail["context_count"], 1);
        store
            .call(|db| {
                assert!(
                    crate::host::check_guard(
                        db,
                        &ContextGuard {
                            conversation_id: "group".into(),
                            epoch: 2
                        }
                    )
                    .is_err()
                );
                crate::host::check_guard(
                    db,
                    &ContextGuard {
                        conversation_id: "group".into(),
                        epoch: 3,
                    },
                )?;
                assert_eq!(
                    db.query_row(
                        "SELECT summary FROM ai_conversations WHERE id='group'",
                        [],
                        |r| r.get::<_, String>(0)
                    )?,
                    "原有摘要"
                );
                assert_eq!(
                    db.query_row(
                        "SELECT count(*) FROM ai_memories WHERE conversation_id='group'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    1
                );
                Ok(())
            })
            .await
            .unwrap();
        change.request_id = "unknown-persona".into();
        change.epoch = 3;
        change.memory_revision = 2;
        change.action = Action::Settings {
            settings: AiSettings {
                persona: "刚保存的自定义".into(),
                ..settings
            },
        };
        let mut saved_config = service.config.clone();
        saved_config
            .personas
            .insert("刚保存的自定义".into(), "新的自定义表达".into());
        let error = service.mutate(&actor, change.clone()).await.unwrap_err();
        assert!(error.to_string().contains("请先应用全局配置"));
        assert_eq!(service.conversation(&actor, "group").await.unwrap(), detail);
        // Only after the runtime adopts that configuration may a conversation
        // select its new custom entry. No failed request changed its generation.
        let applied_service = MemoryAdmin::new(store.clone(), saved_config);
        applied_service.mutate(&actor, change).await.unwrap();
        assert_eq!(
            applied_service.conversation(&actor, "group").await.unwrap()["settings"]["persona"],
            "刚保存的自定义"
        );
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn settings_preserve_context_reset_hides_it_and_mode_revisions_do_not_revive_it() {
        let (_dir, store, service, actor) = fixture().await;
        let settings = AiSettings {
            paused: true,
            once: true,
            ..Default::default()
        };
        service
            .mutate(
                &actor,
                MutationRequest {
                    request_id: "settings".into(),
                    conversation_id: "group".into(),
                    epoch: 1,
                    memory_revision: 0,
                    action: Action::Settings { settings },
                },
            )
            .await
            .unwrap();
        let detail = service.conversation(&actor, "group").await.unwrap();
        assert_eq!(detail["context_count"], 1);
        assert_eq!(detail["settings"]["paused"], true);
        assert_eq!(detail["settings"]["once"], true);
        service
            .mutate(
                &actor,
                MutationRequest {
                    request_id: "reset".into(),
                    conversation_id: "group".into(),
                    epoch: 2,
                    memory_revision: 1,
                    action: Action::Reset,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            service.conversation(&actor, "group").await.unwrap()["context_count"],
            0
        );
        for (index, mode) in ["user", "group"].into_iter().enumerate() {
            service
                .mutate(
                    &actor,
                    MutationRequest {
                        request_id: format!("mode-{index}"),
                        conversation_id: "group".into(),
                        epoch: 3 + index as i64,
                        memory_revision: 2 + index as i64,
                        action: Action::Mode { mode: mode.into() },
                    },
                )
                .await
                .unwrap();
            let detail = service.conversation(&actor, "group").await.unwrap();
            assert_eq!(detail["scope_mode"], mode);
            assert_eq!(detail["scope_revision"], index + 1);
            assert_eq!(detail["context_count"], 0);
        }
        store.stop().await.unwrap();
    }
    #[tokio::test]
    async fn stale_or_expired_previews_never_delete_and_pagination_is_scoped() {
        let (_dir, store, service, actor) = fixture().await;
        let result = service.mutate(&actor, create("one", 1, 0)).await.unwrap();
        let id = result["ids"][0].as_i64().unwrap();
        let cv = service.conversation(&actor, "group").await.unwrap();
        let preview = service
            .preview_forget(&actor, preview_request(&cv, vec![id]))
            .await
            .unwrap();
        let confirm = ForgetConfirmRequest {
            request_id: "stale-delete".into(),
            preview_id: preview["preview_id"].as_str().unwrap().into(),
        };
        service
            .mutate(
                &actor,
                MutationRequest {
                    request_id: "disable".into(),
                    conversation_id: "group".into(),
                    epoch: 2,
                    memory_revision: 1,
                    action: Action::SetActive {
                        id,
                        version: 1,
                        active: false,
                    },
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            service
                .confirm_forget(&actor, confirm)
                .await
                .unwrap_err()
                .downcast_ref::<AdminError>(),
            Some(AdminError::Conflict(_))
        ));
        let cv = service.conversation(&actor, "group").await.unwrap();
        let preview = service
            .preview_forget(&actor, preview_request(&cv, vec![id]))
            .await
            .unwrap();
        store.call(|db| { db.execute("UPDATE ai_memory_previews SET expires_at=0",[])?;for i in 0..45 { db.execute("INSERT INTO ai_memories(conversation_id,subject_id,kind,text,evidence,created_at,updated_at) VALUES('group',8,'fact',?1,'[]',1,1)",[format!("分页记录{i}")])?; }Ok(()) }).await.unwrap();
        assert!(
            service
                .confirm_forget(
                    &actor,
                    ForgetConfirmRequest {
                        request_id: "expired-delete".into(),
                        preview_id: preview["preview_id"].as_str().unwrap().into()
                    }
                )
                .await
                .is_err()
        );
        assert!(service.detail(&actor, id, None).await.is_ok());
        let page = service
            .memories(
                &actor,
                MemoryQuery {
                    conversation_id: "group".into(),
                    page: Some(3),
                    keyword: Some("分页".into()),
                    subject_id: Some(8),
                    active: Some(true),
                },
            )
            .await
            .unwrap();
        assert_eq!(page["total"], 45);
        assert_eq!(page["items"].as_array().unwrap().len(), 5);
        let groups = service
            .conversations(
                &actor,
                ConversationQuery {
                    bot_id: Some(1),
                    peer_id: Some(10),
                    scene: Some("group".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(groups["total"], 1);
        store.stop().await.unwrap();
    }
}
