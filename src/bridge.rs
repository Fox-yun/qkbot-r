//! Optional Milky-compatible ingress for one trusted NoneBot process.
//! It cannot choose arbitrary destinations, inherit QQ credentials, or bypass the outbox.
use crate::{
    host::Host,
    milky::SendOutcome,
    types::{Incoming, OutgoingMessage, Scene, now},
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Request, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Semaphore, mpsc};
use uuid::Uuid;

#[derive(Clone)]
struct Task {
    id: String,
    generation: String,
    plugin: String,
    message: Incoming,
    position: i64,
    access_revision: i64,
    plugin_revision: i64,
    expires: i64,
}
#[derive(Default)]
struct Session {
    generation: String,
    sender: Option<mpsc::Sender<Value>>,
    tasks: HashMap<String, Task>,
    leases: HashMap<(String, i64), (String, i64)>,
}
struct Media {
    path: PathBuf,
    mime: String,
    task: Task,
    size: usize,
    _temporary: tempfile::TempPath,
}
pub struct Bridge {
    session: Mutex<Session>,
    operations: tokio::sync::Mutex<()>,
    requests: Arc<Semaphore>,
    uploads: Arc<Semaphore>,
    media: Mutex<HashMap<String, Media>>,
    patterns: std::sync::OnceLock<Vec<(String, regex::RegexSet)>>,
}
impl Default for Bridge {
    fn default() -> Self {
        Self {
            session: Mutex::new(Session::default()),
            operations: tokio::sync::Mutex::new(()),
            requests: Arc::new(Semaphore::new(16)),
            uploads: Arc::new(Semaphore::new(2)),
            media: Mutex::new(HashMap::new()),
            patterns: Default::default(),
        }
    }
}
impl Bridge {
    fn validate_media(&self, host: &Host, task: &Task, message: &OutgoingMessage) -> Result<()> {
        let mut uris = vec![];
        crate::media::media_uris(&message.segments, &mut uris);
        let base = reqwest::Url::parse(&format!("http://{}/", host.config.nonebot.listen))?;
        let media = self.media.lock().unwrap();
        for uri in uris {
            let parsed = reqwest::Url::parse(uri)?;
            let name = parsed.host_str().unwrap_or("");
            let local=name.eq_ignore_ascii_case("localhost")||name.ends_with(".localhost")
                ||name.trim_matches(['[',']']).parse::<std::net::IpAddr>().ok().is_some_and(|ip|
                    ip.is_loopback()||matches!(ip,std::net::IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some_and(|v4|v4.is_loopback())));
            if local || parsed.origin() == base.origin() {
                ensure!(
                    parsed.origin() == base.origin() && parsed.query().is_none(),
                    "本机媒体必须通过受控上传入口"
                );
                let id = parsed
                    .path()
                    .strip_prefix("/media/")
                    .context("外部插件不能访问原生插件资源")?;
                let entry = media.get(id).context("上传媒体不存在")?;
                ensure!(
                    entry.task.id == task.id
                        && entry.task.generation == task.generation
                        && entry.task.plugin == task.plugin,
                    "媒体不属于本次任务"
                );
            }
        }
        Ok(())
    }
    pub async fn selected(&self, host: &Host, message: &Incoming) -> Result<Option<String>> {
        let root = message.text.split_whitespace().next().unwrap_or("");
        // An explicitly registered but disabled external command is still claimed.
        if let Some(plugin) = host
            .config
            .nonebot
            .plugins
            .iter()
            .find(|p| p.commands.iter().any(|c| c == root))
        {
            return Ok(Some(plugin.id.clone()));
        }
        let patterns = self.patterns.get_or_init(|| {
            host.config
                .nonebot
                .plugins
                .iter()
                .filter_map(|p| {
                    regex::RegexSet::new(&p.patterns)
                        .ok()
                        .map(|r| (p.id.clone(), r))
                })
                .collect()
        });
        if let Some((id, _)) = patterns
            .iter()
            .find(|(_, patterns)| patterns.is_match(&message.text))
        {
            return Ok(Some(id.clone()));
        }
        if !host.config.nonebot.enabled {
            return Ok(None);
        }
        let lease = {
            let mut s = self.session.lock().unwrap();
            s.leases.retain(|_, (_, until)| *until > now());
            s.leases
                .get(&(message.key.scope.key(), message.sender_id))
                .and_then(|(id, _)| s.tasks.get(id))
                .cloned()
        };
        if let Some(task) = lease
            && self.validate(host, &task).await.is_ok()
        {
            return Ok(Some(task.plugin));
        }
        let triggers = url_trigger_text(message);
        for plugin in host.config.nonebot.plugins.iter().filter(|p| p.enabled) {
            if triggers
                .iter()
                .any(|text| has_url_host(text, &plugin.url_hosts))
            {
                return Ok(Some(plugin.id.clone()));
            }
        }
        Ok(None)
    }
    pub async fn publish(
        &self,
        host: &Host,
        message: Incoming,
        position: i64,
        plugin: String,
    ) -> Result<()> {
        ensure!(host.config.nonebot.enabled, "NoneBot接入未开启");
        ensure!(
            host.config
                .nonebot
                .plugins
                .iter()
                .any(|p| p.id == plugin && p.enabled),
            "外部插件未启用"
        );
        let scope = message.key.scope.key();
        let origin = plugin.clone();
        let source = message.key.key();
        let (access_revision, plugin_revision) = host
            .store
            .call(move |db| {
                crate::catalog::check_message(db, &source, &origin)?;
                Ok((
                    crate::access::require_enabled(db, &scope)?,
                    crate::catalog::allowed_revision(db, &scope, &origin)?.context("插件已关闭")?,
                ))
            })
            .await?;
        let mut s = self.session.lock().unwrap();
        s.tasks.retain(|_, t| t.expires > now());
        ensure!(s.tasks.len() < 256, "外部插件任务已满");
        let sender = s.sender.clone().context("NoneBot未连接，请启动插件进程")?;
        let task = Task {
            id: Uuid::new_v4().to_string(),
            generation: s.generation.clone(),
            plugin,
            message,
            position,
            access_revision,
            plugin_revision,
            expires: now() + host.config.nonebot.task_timeout_secs as i64,
        };
        let mut raw = task.message.raw_message.clone();
        if raw.is_null() {
            raw = json!({"message_scene":task.message.key.scope.scene.as_str(),"peer_id":task.message.key.scope.peer_id,"message_seq":task.message.key.message_seq,
                "sender_id":task.message.sender_id,"time":task.message.received_at,"segments":[{"type":"text","data":{"text":task.message.text}}]});
        }
        let event = json!({"time":task.message.received_at,"self_id":task.message.key.scope.bot_id,"event_type":"message_receive","data":raw,
            "qkbot":{"task_id":task.id,"generation":task.generation,"plugin_id":task.plugin}});
        sender
            .try_send(event)
            .map_err(|_| anyhow::anyhow!("NoneBot未连接或处理队列已满"))?;
        s.tasks.insert(task.id.clone(), task);
        Ok(())
    }
    async fn validate(&self, host: &Host, task: &Task) -> Result<()> {
        ensure!(
            !host.cancel.is_cancelled() && task.expires > now(),
            "外部任务已过期"
        );
        ensure!(
            host.milky.verified_bot_id() == Some(task.message.key.scope.bot_id),
            "机器人账号未验证"
        );
        {
            let s = self.session.lock().unwrap();
            ensure!(
                s.sender.is_some() && s.generation == task.generation,
                "NoneBot运行代次已变化"
            );
        }
        let scope = task.message.key.scope.key();
        let source = task.message.key.key();
        let plugin = task.plugin.clone();
        let access = task.access_revision;
        let revision = task.plugin_revision;
        let epoch = *host.epochs.get(&plugin).context("外部插件已关闭")?;
        host.store
            .call(move |db| {
                crate::access::check_revision(db, &scope, access)?;
                crate::catalog::check_revision(db, &scope, &plugin, revision)?;
                crate::catalog::check_message(db, &source, &plugin)?;
                crate::host::check_plugin(db, &plugin, epoch)
            })
            .await
    }
    fn task(&self, identity: &Identity) -> Result<Task> {
        let s = self.session.lock().unwrap();
        let task = s.tasks.get(&identity.task_id).context("未知外部任务")?;
        ensure!(
            task.plugin == identity.plugin_id
                && task.generation == identity.generation
                && s.generation == identity.generation,
            "外部任务身份不匹配"
        );
        Ok(task.clone())
    }
}

fn url_trigger_text(message: &Incoming) -> Vec<String> {
    let mut candidates = vec![message.text.clone()];
    if let Some(segments) = message.raw_message["segments"].as_array() {
        for segment in segments {
            if segment["type"] != "light_app" {
                continue;
            }
            let Some(payload) = segment["data"]["json_payload"]
                .as_str()
                .filter(|s| s.len() <= 16 * 1024)
            else {
                continue;
            };
            let Ok(card) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            for (kind, field) in [
                ("detail_1", "qqdocurl"),
                ("news", "jumpUrl"),
                ("music", "jumpUrl"),
            ] {
                if let Some(url) = card["meta"][kind][field]
                    .as_str()
                    .filter(|s| s.len() <= 4096)
                {
                    candidates.push(url.to_owned());
                }
            }
        }
    }
    candidates
}
fn has_url_host(text: &str, hosts: &[String]) -> bool {
    text.split(|c: char| c.is_whitespace() || matches!(c, '"' | '<' | '>' | '\\' | '（' | '）'))
        .any(|candidate| {
            let start = candidate
                .find("https://")
                .or_else(|| candidate.find("http://"));
            start
                .and_then(|start| reqwest::Url::parse(&candidate[start..]).ok())
                .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
                .is_some_and(|host| {
                    hosts.iter().any(|allowed| {
                        let allowed = allowed.to_ascii_lowercase();
                        host == allowed || host.ends_with(&format!(".{allowed}"))
                    })
                })
        })
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    task_id: String,
    plugin_id: String,
    generation: String,
    #[serde(default)]
    operation_id: String,
}

pub fn router(host: Arc<Host>) -> Router {
    Router::new()
        .route("/event", get(events))
        .route("/api/{api}", post(api))
        .route("/qkbot/catalog", get(catalog))
        .route("/qkbot/lease", post(lease))
        .route("/qkbot/media", post(upload))
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(host.clone(), authorize))
        .route("/media/{id}", get(download))
        .route("/native-media/{id}", get(crate::media::download))
        .with_state(host)
}
pub async fn serve(host: Arc<Host>, listener: tokio::net::TcpListener) -> Result<()> {
    axum::serve(listener, router(host.clone()))
        .with_graceful_shutdown(host.cancel.clone().cancelled_owned())
        .await?;
    Ok(())
}
async fn authorize(State(host): State<Arc<Host>>, request: Request, next: Next) -> Response {
    let config = &host.config.nonebot;
    if !config.enabled || config.token.len() < 32 {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "NoneBot入口未启用");
    }
    let expected = format!("Bearer {}", config.token);
    if request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some(expected.as_str())
    {
        return failure(StatusCode::UNAUTHORIZED, "鉴权失败");
    }
    let Ok(_permit) = host.bridge.requests.clone().try_acquire_owned() else {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "请求过多");
    };
    next.run(request).await
}
fn failure(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({"status":"failed","retcode":status.as_u16(),"message":message})),
    )
        .into_response()
}
fn success(data: Value) -> Response {
    Json(json!({"status":"ok","retcode":0,"data":data})).into_response()
}
async fn catalog(State(host): State<Arc<Host>>) -> Json<Value> {
    Json(json!({"plugins":host.config.nonebot.plugins}))
}
async fn events(State(host): State<Arc<Host>>, ws: WebSocketUpgrade) -> Response {
    // Reserve the single socket before upgrading; a second client cannot invalidate it.
    let (sender, receiver) = mpsc::channel(host.config.nonebot.queue_capacity);
    let generation = Uuid::new_v4().to_string();
    {
        let mut s = host.bridge.session.lock().unwrap();
        if s.sender.is_some() {
            return failure(StatusCode::CONFLICT, "已有NoneBot连接");
        }
        *s = Session {
            generation: generation.clone(),
            sender: Some(sender),
            ..Default::default()
        };
    }
    let failure_host = host.clone();
    let failure_generation = generation.clone();
    ws.max_message_size(8192)
        .on_failed_upgrade(move |_| {
            let mut s = failure_host.bridge.session.lock().unwrap();
            if s.generation == failure_generation {
                *s = Session::default();
            }
        })
        .on_upgrade(move |socket| socket_loop(host, socket, receiver, generation))
        .into_response()
}
async fn socket_loop(
    host: Arc<Host>,
    socket: WebSocket,
    mut receiver: mpsc::Receiver<Value>,
    generation: String,
) {
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            _=host.cancel.cancelled()=>break,
            frame=stream.next()=>match frame {Some(Ok(Message::Ping(bytes)))=>if !matches!(tokio::time::timeout(Duration::from_secs(5),sink.send(Message::Pong(bytes))).await,Ok(Ok(()))){break},Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break,_=>{}},
            event=receiver.recv()=>match event {Some(event)=>if !matches!(tokio::time::timeout(Duration::from_secs(5),sink.send(Message::Text(event.to_string().into()))).await,Ok(Ok(()))){break},None=>break}
        }
    }
    let mut s = host.bridge.session.lock().unwrap();
    if s.generation == generation {
        *s = Session::default();
    }
}
async fn read_json(request: Request) -> Result<Value> {
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        axum::body::to_bytes(request.into_body(), 256 * 1024),
    )
    .await
    .context("请求超时")?
    .context("请求过大")?;
    Ok(serde_json::from_slice(&bytes)?)
}
async fn api(
    State(host): State<Arc<Host>>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let result=async {
        let mut body=read_json(request).await?;
        ensure!(body.is_object(),"API参数须为对象");
        if matches!(name.as_str(),"get_login_info"|"get_impl_info") {
            body.as_object_mut().unwrap().remove("qkbot");
            return host.milky.proxy_read(None,&name,body).await;
        }
        let identity:Identity=serde_json::from_value(body.as_object_mut().unwrap().remove("qkbot").context("缺少任务身份")?)?;
        let task=host.bridge.task(&identity)?;
        host.bridge.validate(&host,&task).await?;
        let scope=&task.message.key.scope;
        let result=match name.as_str() {
            "send_group_message"|"send_private_message"=>{
                let group=name=="send_group_message";
                ensure!((group && scope.scene==Scene::Group)||(!group&&scope.scene==Scene::Friend),"发送场景不匹配");
                ensure!(body[if group{"group_id"}else{"user_id"}].as_i64()==Some(scope.peer_id),"禁止跨会话发送");
                ensure!(!identity.operation_id.is_empty() && identity.operation_id.len()<=128 && identity.operation_id.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_".contains(&b)),"发送操作ID无效");
                let message:OutgoingMessage=serde_json::from_value(json!({"segments":body["message"]}))?;
                message.validate(host.config.limits.max_message_bytes)?;
                host.bridge.validate_media(&host,&task,&message)?;
                // Serialize submissions so a duplicated operation cannot race a first attempt.
                let _serial=host.bridge.operations.lock().await;
                host.bridge.validate(&host,&task).await?;
                let id=format!("nb:{:x}",Sha256::digest(format!("{}:{}",task.id,identity.operation_id).as_bytes()));
                let expected=serde_json::to_string(&message)?;
                let lookup=id.clone();
                let existing:Option<(String,String,Option<i64>)>=host.store.call(move|db|Ok(db.query_row("SELECT state,body,message_seq FROM outbox WHERE id=?1",[lookup],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?)).await?;
                if let Some((state,previous,seq))=existing {
                    ensure!(previous==expected,"操作ID已用于不同消息");
                    ensure!(state=="confirmed","此发送已有记录；结果未知或失败，不重复提交");
                    return Ok(json!({"message_seq":seq.context("发送回执缺失")?,"time":now()}));
                }
                match host.send_message_with_id(scope,&task.plugin,None,task.position,message,id).await? {
                    SendOutcome::Confirmed{message_seq}=>Ok(json!({"message_seq":message_seq,"time":now()})),
                    SendOutcome::Deferred{..}=>anyhow::bail!("发送未提交，账号或权限暂不可用"),
                    SendOutcome::Failed{..}=>anyhow::bail!("协议端拒绝发送"),
                    SendOutcome::Unknown{..}=>anyhow::bail!("发送结果未知，禁止自动重试"),
                }
            }
            "send_group_message_reaction"|"upload_group_file"|"upload_private_file"=>{
                ensure!(!identity.operation_id.is_empty()&&identity.operation_id.len()<=128
                    &&identity.operation_id.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_".contains(&b)),"操作ID无效");
                if name=="send_group_message_reaction" {
                    ensure!(body["message_seq"].as_i64()==Some(task.message.key.message_seq),"只能回应本次触发消息");
                } else {
                    let prefix=format!("http://{}/media/",host.config.nonebot.listen);
                    let id=body["file_uri"].as_str().and_then(|uri|uri.strip_prefix(&prefix)).context("文件必须先通过受控媒体入口上传")?;
                    let media=host.bridge.media.lock().unwrap();
                    let entry=media.get(id).context("上传文件不存在")?;
                    ensure!(entry.task.id==task.id && entry.task.plugin==task.plugin,"文件不属于本次任务");
                }
                execute_action(&host,&task,&identity.operation_id,&name,body).await
            }
            "get_message"=>{
                ensure!(body["message_scene"].as_str()==Some(scope.scene.as_str()) && body["peer_id"].as_i64()==Some(scope.peer_id),"禁止跨会话查询");
                let seq=body["message_seq"].as_i64().filter(|n|*n>0).context("消息序号无效")?;
                Ok(json!({"message":host.milky.get_message_raw(&crate::types::MessageKey{scope:scope.clone(),message_seq:seq}).await?}))
            }
            "get_group_member_list"=>{
                ensure!(scope.scene==Scene::Group && body["group_id"].as_i64()==Some(scope.peer_id),"群号不匹配");
                let list=host.milky.get_group_members(scope,false).await?;
                Ok(json!({"members":list.into_iter().map(|member|member.raw).collect::<Vec<_>>()}))
            }
            "get_group_member_info"=>{
                ensure!(scope.scene==Scene::Group && body["group_id"].as_i64()==Some(scope.peer_id),"群号不匹配");
                let user=body["user_id"].as_i64().filter(|n|*n>0).context("用户ID无效")?;
                Ok(json!({"member":host.milky.get_member(scope,user,false).await?.raw}))
            }
            "get_group_info"|"get_friend_info"=>host.milky.proxy_read(Some(scope),&name,body).await,
            "get_resource_temp_url"=>{
                let resource=body["resource_id"].as_str().context("资源ID缺失")?;
                let mut url=host.milky.get_resource_temp_url(&task.message.key,resource).await;
                if url.is_err() && let Some(seq)=task.message.quote_seq {
                    url=host.milky.get_resource_temp_url(&crate::types::MessageKey{scope:scope.clone(),message_seq:seq},resource).await;
                }
                Ok(json!({"url":url?}))
            }
            _=>anyhow::bail!("此API未开放"),
        };
        // Read responses may arrive after a toggle or a socket reconnect.
        // A confirmed write keeps its real receipt even if permissions change.
        if !matches!(name.as_str(),"send_group_message"|"send_private_message"|"send_group_message_reaction"|"upload_group_file"|"upload_private_file") {
            host.bridge.validate(&host,&task).await?;
        }
        result
    }.await;
    match result {
        Ok(data) => success(data),
        Err(error) => {
            // All bridge errors presented here are local validation or sanitized Milky errors.
            tracing::debug!(api=%name,"NoneBot request rejected");
            failure(StatusCode::BAD_REQUEST, &error.to_string())
        }
    }
}

async fn execute_action(
    host: &Host,
    task: &Task,
    operation: &str,
    api: &str,
    body: Value,
) -> Result<Value> {
    use crate::milky::ApiOutcome;
    let _serial = host.bridge.operations.lock().await;
    let _delivery = host.access.delivery_guard(&task.message.key.scope).await?;
    host.bridge.validate(host, task).await?;
    let id = format!(
        "nb:{:x}",
        Sha256::digest(format!("{}:{operation}", task.id).as_bytes())
    );
    let encoded = json!({"api":api,"params":body}).to_string();
    let (lookup, expected) = (id.clone(), encoded.clone());
    let existing = host
        .store
        .call(move |db| {
            Ok(db
                .query_row(
                    "SELECT state,body,detail FROM outbox WHERE id=?1",
                    [lookup],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()?)
        })
        .await?;
    if let Some((state, previous, result)) = existing {
        ensure!(previous == expected, "操作ID已用于不同请求");
        ensure!(state == "confirmed", "操作已有记录，禁止自动重复提交");
        return Ok(serde_json::from_str(result.as_deref().unwrap_or("null"))?);
    }
    let (record, scope, plugin, source) = (
        id.clone(),
        task.message.key.scope.key(),
        task.plugin.clone(),
        task.message.key.key(),
    );
    let revision = task.access_revision;
    let plugin_revision = task.plugin_revision;
    let epoch = *host.epochs.get(&plugin).context("插件已停用")?;
    let hourly = host.config.limits.sends_per_hour as i64;
    host.store.call(move|db| {
        let tx=db.transaction()?;
        crate::access::check_revision(&tx,&scope,revision)?;
        crate::catalog::check_revision(&tx,&scope,&plugin,plugin_revision)?;
        crate::catalog::check_message(&tx,&source,&plugin)?;
        crate::host::check_plugin(&tx,&plugin,epoch)?;
        let count:i64=tx.query_row("SELECT COUNT(*) FROM outbox WHERE scope=?1 AND created_at>?2 AND state IN('prepared','submitted','confirmed','unknown')",params![scope,now()-3600],|r|r.get(0))?;
        ensure!(count<hourly,"本小时发送额度已用完");
        tx.execute("INSERT INTO outbox(id,scope,origin_plugin,plugin_epoch,body,state,created_at,updated_at,access_revision) VALUES(?1,?2,?3,?4,?5,'submitted',?6,?6,?7)",params![record,scope,plugin,epoch,encoded,now(),revision])?;
        tx.commit()?;Ok(())
    }).await?;
    let outcome = host
        .milky
        .bridge_action(&task.message.key.scope, api, body, &host.cancel)
        .await;
    let (state, result) = match &outcome {
        Ok(ApiOutcome::Confirmed(value)) => ("confirmed", Some(value.clone())),
        Ok(ApiOutcome::Unknown) => ("unknown", None),
        _ => ("failed", None),
    };
    let saved = result
        .as_ref()
        .map(serde_json::Value::to_string)
        .unwrap_or_default();
    if let Err(error) = host
        .store
        .call(move |db| {
            db.execute(
                "UPDATE outbox SET state=?2,detail=?3,updated_at=?4 WHERE id=?1",
                params![id, state, saved, now()],
            )?;
            Ok(())
        })
        .await
    {
        tracing::error!(%error,"plugin action result could not be persisted; submitted intent is retained");
    }
    match outcome {
        Ok(ApiOutcome::Confirmed(value)) => Ok(value),
        Ok(ApiOutcome::Unknown) => anyhow::bail!("操作结果未知，禁止自动重试"),
        _ => anyhow::bail!("操作未确认成功"),
    }
}
async fn lease(State(host): State<Arc<Host>>, request: Request) -> Response {
    let result = async {
        let mut body = read_json(request).await?;
        let seconds = body
            .as_object_mut()
            .context("租约参数无效")?
            .remove("seconds")
            .and_then(|v| v.as_i64())
            .context("租约秒数缺失")?;
        ensure!((0..=120).contains(&seconds), "租约期限为0到120秒");
        let identity: Identity = serde_json::from_value(body)?;
        let task = host.bridge.task(&identity)?;
        host.bridge.validate(&host, &task).await?;
        let key = (task.message.key.scope.key(), task.message.sender_id);
        let mut s = host.bridge.session.lock().unwrap();
        if seconds == 0 {
            s.leases.remove(&key);
        } else {
            s.leases
                .insert(key, (task.id, task.expires.min(now() + seconds)));
        }
        anyhow::Ok(json!({"accepted":true}))
    }
    .await;
    match result {
        Ok(data) => Json(data).into_response(),
        Err(_) => failure(StatusCode::BAD_REQUEST, "多轮会话任务无效"),
    }
}
fn header_identity(headers: &HeaderMap) -> Result<Identity> {
    let get = |key: &str| {
        headers
            .get(key)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .context("上传任务身份缺失")
    };
    Ok(Identity {
        task_id: get("x-qkbot-task")?,
        plugin_id: get("x-qkbot-plugin")?,
        generation: get("x-qkbot-generation")?,
        operation_id: String::new(),
    })
}
async fn upload(State(host): State<Arc<Host>>, request: Request) -> Response {
    let result=async {
        let _permit=host.bridge.uploads.clone().try_acquire_owned().context("上传繁忙")?;
        let task=host.bridge.task(&header_identity(request.headers())?)?;
        host.bridge.validate(&host,&task).await?;
        cleanup(&host).await;
        let mime=request.headers().get(header::CONTENT_TYPE).and_then(|v|v.to_str().ok()).unwrap_or("application/octet-stream").to_owned();
        ensure!(matches!(mime.as_str(),"image/png"|"image/jpeg"|"image/gif"|"image/webp"|"audio/mpeg"|"audio/ogg"|"audio/wav"|"audio/mp4"|"audio/aac"|"audio/flac"|"audio/amr"|"audio/silk"|"video/mp4"|"application/octet-stream"),"不支持的媒体类型");
        // Files stream to disk; increasing the configured file quota must not
        // silently hit an unrelated smaller fixed cache ceiling.
        let cache_limit=(128*1024*1024).max(host.config.nonebot.media_max_bytes.saturating_mul(2));
        {
            let media=host.bridge.media.lock().unwrap();
            ensure!(media.len()<32 && media.values().map(|m|m.size).sum::<usize>()<cache_limit,"媒体缓存已满");
        }
        let directory=PathBuf::from(&host.config.nonebot.media_dir);
        tokio::fs::create_dir_all(&directory).await?;
        let id=Uuid::new_v4().simple().to_string();
        // TempPath removes incomplete uploads when a request future is dropped.
        let temporary=tempfile::Builder::new().prefix(".qkbot-upload-").suffix(".media").tempfile_in(&directory)?;
        let mut file=tokio::fs::File::from_std(temporary.reopen()?);
        let temporary=temporary.into_temp_path();
        let path=temporary.to_path_buf();
        let mut size=0usize;
        let mut stream=request.into_body().into_data_stream();
        let copying=async {
            while let Some(chunk)=stream.next().await {
                let chunk=chunk?;size=size.checked_add(chunk.len()).context("媒体长度无效")?;
                ensure!(size<=host.config.nonebot.media_max_bytes,"媒体文件过大");
                file.write_all(&chunk).await?;
            }
            ensure!(size>0,"媒体不能为空");
            file.flush().await?;
            host.bridge.validate(&host,&task).await?;
            anyhow::Ok(())
        };
        let copy=tokio::select! {
            _=host.cancel.cancelled()=>Err(anyhow::anyhow!("宿主正在停止")),
            result=tokio::time::timeout(Duration::from_secs(host.config.nonebot.task_timeout_secs.clamp(30,300)),copying)=>result.unwrap_or_else(|_|Err(anyhow::anyhow!("上传超时"))),
        };
        drop(file);
        if let Err(error)=copy {let _=tokio::fs::remove_file(&path).await;return Err(error);}
        let registered={
            let mut media=host.bridge.media.lock().unwrap();
            if media.len()<32 && media.values().map(|m|m.size).sum::<usize>()+size<=cache_limit {
                media.insert(id.clone(),Media{path:path.clone(),mime,task,size,_temporary:temporary});true
            } else {false}
        };
        if !registered {let _=tokio::fs::remove_file(path).await;anyhow::bail!("媒体缓存已满");}
        Ok(json!({"media_id":id,"url":format!("http://{}/media/{id}",host.config.nonebot.listen)}))
    }.await;
    match result {
        Ok(data) => Json(data).into_response(),
        Err(_) => failure(
            StatusCode::BAD_REQUEST,
            "媒体上传失败，检查任务、格式和大小限制",
        ),
    }
}
async fn download(
    State(host): State<Arc<Host>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !crate::media::local_authority(&headers, &host) {
        return failure(StatusCode::NOT_FOUND, "媒体不存在");
    }
    let entry = host
        .bridge
        .media
        .lock()
        .unwrap()
        .get(&id)
        .map(|m| (m.path.clone(), m.mime.clone(), m.task.clone(), m.size));
    let Some((path, mime, task, size)) = entry else {
        return failure(StatusCode::NOT_FOUND, "媒体不存在");
    };
    if host.bridge.validate(&host, &task).await.is_err() {
        return failure(StatusCode::NOT_FOUND, "媒体已失效");
    }
    match tokio::fs::File::open(path).await {
        Ok(file) => {
            use tokio::io::AsyncReadExt;
            if file.metadata().await.map(|m| m.len()).ok() != Some(size as u64)
                || host.bridge.validate(&host, &task).await.is_err()
            {
                return failure(StatusCode::NOT_FOUND, "媒体已失效");
            }
            let stream = tokio_util::io::ReaderStream::new(file.take(size as u64));
            (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-store".into()),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
                ],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Err(_) => failure(StatusCode::NOT_FOUND, "媒体不存在"),
    }
}
pub async fn cleanup(host: &Host) {
    let generation = host.bridge.session.lock().unwrap().generation.clone();
    let expired: Vec<_> = {
        let mut media = host.bridge.media.lock().unwrap();
        let ids: Vec<_> = media
            .iter()
            .filter(|(_, m)| {
                m.task.expires <= now()
                    || host.cancel.is_cancelled()
                    || m.task.generation != generation
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| media.remove(&id))
            .map(|m| m.path)
            .collect()
    };
    for path in expired {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn incoming(raw: Value, text: &str) -> Incoming {
        Incoming {
            key: crate::types::MessageKey {
                scope: crate::types::Scope {
                    bot_id: 1,
                    scene: Scene::Group,
                    peer_id: 2,
                },
                message_seq: 3,
            },
            sender_id: 4,
            sender_nickname: None,
            sender_card: None,
            text: text.into(),
            mentions: vec![],
            quote_seq: None,
            received_at: now(),
            raw_message: raw,
            notification: None,
        }
    }
    #[test]
    fn url_trigger_uses_only_plain_text_and_known_light_app_fields() {
        let card = json!({"meta":{"detail_1":{"qqdocurl":"https://b23.tv/a"},"news":{"jumpUrl":"https://www.bilibili.com/b"},"music":{"jumpUrl":"https://www.bilibili.com/c"}},
            "unknown":"https://evil.test/hidden"});
        let message = incoming(
            json!({"nickname":"https://b23.tv/not-a-command","segments":[
                {"type":"image","data":{"url":"https://b23.tv/not-a-card"}},
                {"type":"light_app","data":{"json_payload":card.to_string()}},
                {"type":"light_app","data":{"json_payload":"invalid"}}
            ]}),
            "https://www.bilibili.com/plain",
        );
        let candidates = url_trigger_text(&message);
        assert_eq!(candidates.len(), 4);
        assert!(
            !candidates
                .iter()
                .any(|text| text.contains("not-a") || text.contains("evil.test"))
        );
        assert!(
            candidates
                .iter()
                .any(|text| has_url_host(text, &["b23.tv".into()]))
        );
        let ignored = incoming(
            json!({"unused":"https://b23.tv/hidden","segments":[{"type":"light_app","data":{"json_payload":json!({"url":"https://b23.tv/hidden"}).to_string()}}]}),
            "hello",
        );
        assert!(
            !url_trigger_text(&ignored)
                .iter()
                .any(|text| has_url_host(text, &["b23.tv".into()]))
        );
        assert!(!has_url_host(
            "https://b23.tv.attacker.test/x",
            &["b23.tv".into()]
        ));
        assert!(has_url_host(
            "https://www.bilibili.com/video/BV1xx411c7mD",
            &["bilibili.com".into()]
        ));
        assert!(!has_url_host(
            "https://notbilibili.com/video",
            &["bilibili.com".into()]
        ));
    }
}
