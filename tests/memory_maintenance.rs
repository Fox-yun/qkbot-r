//! Idle learning, bounded repair, and fairness use only local synthetic services.
use axum::{Json, Router, extract::State, routing::post};
use qkbot::{config::Config, host::Host, plugins::ai::chat::Chat, types::ContextGuard};
use rusqlite::params;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

#[derive(Default)]
struct RepairBarrier {
    entered: Notify,
    release: Notify,
}

type Respond = dyn Fn(&Value) -> String + Send + Sync;
#[derive(Clone)]
struct Model {
    inputs: Arc<Mutex<Vec<Value>>>,
    respond: Arc<Respond>,
    repair_barrier: Arc<Mutex<Option<Arc<RepairBarrier>>>>,
}
async fn model(State(state): State<Model>, Json(body): Json<Value>) -> Json<Value> {
    let content = body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    let input: Value = serde_json::from_str(content).unwrap();
    let barrier = state.repair_barrier.lock().unwrap().clone();
    if input.get("next_part").is_some()
        && let Some(barrier) = barrier
    {
        barrier.entered.notify_one();
        barrier.release.notified().await;
    }
    let answer = (state.respond)(&input);
    state.inputs.lock().unwrap().push(input);
    Json(
        json!({"model":"mock","choices":[{"finish_reason":"stop","message":{"role":"assistant","content":answer}}],"usage":{"prompt_tokens":20,"completion_tokens":10}}),
    )
}
async fn setup(
    configure: impl FnOnce(&mut Config),
    respond: impl Fn(&Value) -> String + Send + Sync + 'static,
) -> (Arc<Host>, Arc<Chat>, Model, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let state = Model {
        inputs: Arc::new(Mutex::new(vec![])),
        respond: Arc::new(respond),
        repair_barrier: Arc::new(Mutex::new(None)),
    };
    let router = Router::new()
        .route("/model", post(model))
        .route(
            "/api/get_login_info",
            post(|| async { Json(json!({"status":"ok","retcode":0,"data":{"uin":1}})) }),
        )
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut config = Config::default();
    config.app.database = ":memory:".into();
    config.milky.url = url.clone();
    config.llm.chat_completions_url = format!("{url}/model");
    config.llm.model = "mock".into();
    configure(&mut config);
    let host = Host::create(config).await.unwrap();
    host.milky.login().await.unwrap();
    let chat = Chat::new(host.clone());
    (host, chat, state, server)
}
fn seed(
    db: &rusqlite::Connection,
    id: &str,
    bot: i64,
    learnable: bool,
    settings: &str,
    position: i64,
) -> anyhow::Result<()> {
    let scope = format!("{bot}:group:2");
    db.execute(
        "INSERT OR IGNORE INTO ai_conversations(id,scope,mode,settings) VALUES(?1,?2,'group',?3)",
        params![id, scope, settings],
    )?;
    db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,learnable,sender_nickname,sender_card) VALUES(?1,?2,?3,1,7,'user','我喜欢白茶',1,?4,?5,'昵称','名片')",params![format!("{id}:{position}"),scope,id,position,learnable])?;
    Ok(())
}
fn fenced(value: Value) -> String {
    format!("```json\n{value}\n```")
}

#[tokio::test]
async fn one_idle_message_becomes_a_fact_without_compacting_recent_context() {
    let (host,chat,model,server)=setup(|_|{},|batch| {
        assert_eq!(batch["summary_enabled"],false);
        assert_eq!(batch["memory_write"],true);
        assert_eq!(batch["sources"][0]["sender_card"],"名片");
        fenced(json!({"summary":"","changes":[{"op":"upsert","id":null,"subject_id":7,"kind":"preference","text":"喜欢白茶","evidence":[{"source_key":batch["sources"][0]["key"],"quote":"喜欢白茶"}],"ttl_days":null}]}))
    }).await;
    host.store
        .call(|db| {
            seed(db, "learn", 1, true, "{}", 1)?;
            seed(db, "disabled", 1, true, "{\"memory_write\":false}", 1)?;
            seed(db, "nonlearnable", 1, false, "{}", 1)?;
            seed(db, "foreign-account", 11, true, "{}", 1)?;
            db.execute(
                "UPDATE ai_conversations SET summary='保留的既有摘要' WHERE id='learn'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    chat.compact_idle().await.unwrap();
    let memory_id = host.store.call(|db| {
        let state:(i64,i64,String)=db.query_row("SELECT compacted_until,memory_processed_until,summary FROM ai_conversations WHERE id='learn'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        assert_eq!(state,(0,1,"保留的既有摘要".into()));
        assert_eq!(db.query_row("SELECT count(*) FROM ai_memories WHERE conversation_id='learn'",[],|r|r.get::<_,i64>(0))?,1);
        let memory:(i64,String,i64,String,String,String)=db.query_row("SELECT id,origin,subject_id,kind,text,evidence FROM ai_memories WHERE conversation_id='learn'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        assert_eq!((memory.1.as_str(),memory.2,memory.3.as_str(),memory.4.as_str()),("auto",7,"preference","喜欢白茶"));
        assert_eq!(serde_json::from_str::<Value>(&memory.5)?,json!([{"source_key":"learn:1","quote":"喜欢白茶"}]));
        let provenance:(String,String)=db.query_row("SELECT source_key,quote FROM ai_memory_provenance WHERE memory_id=?1",[memory.0],|r|Ok((r.get(0)?,r.get(1)?)))?;
        assert_eq!(provenance,("learn:1".into(),"喜欢白茶".into()));
        // A preference can also be recalled by the fallback path. Check the
        // FTS match explicitly so a missing search index cannot pass unnoticed.
        let indexed:i64=db.query_row("SELECT memory_id FROM ai_memory_search WHERE ai_memory_search MATCH ?1",["\"白茶\""],|r|r.get(0))?;
        assert_eq!(indexed,memory.0);
        assert_eq!(db.query_row("SELECT count(*) FROM messages WHERE conversation_id='learn' AND epoch=1 AND revoked=0",[],|r|r.get::<_,i64>(0))?,1);
        assert_eq!(db.query_row("SELECT count(*) FROM ai_conversations WHERE id!='learn' AND memory_processed_until!=0",[],|r|r.get::<_,i64>(0))?,0);
        Ok(memory.0)
    }).await.unwrap();
    let guard = ContextGuard {
        conversation_id: "learn".into(),
        epoch: 1,
    };
    let recalled = host
        .memory
        .retrieve(&guard, 7, "白茶", 5, 1000)
        .await
        .unwrap();
    assert_eq!(recalled.len(), 1);
    assert_eq!(recalled[0].id, memory_id);
    assert_eq!(recalled[0].origin, "auto");
    assert_eq!(recalled[0].subject_id, 7);
    assert_eq!(recalled[0].text, "喜欢白茶");
    assert_eq!(recalled[0].evidence[0].source_key, "learn:1");
    assert_eq!(recalled[0].evidence[0].quote, "喜欢白茶");
    assert!(
        host.memory
            .retrieve(&guard, 8, "白茶", 5, 1000)
            .await
            .unwrap()
            .is_empty()
    );
    chat.compact_idle().await.unwrap();
    assert_eq!(model.inputs.lock().unwrap().len(), 1);
    host.store.call(move |db| {
        let state:(i64,i64,String)=db.query_row("SELECT compacted_until,memory_processed_until,summary FROM ai_conversations WHERE id='learn'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        assert_eq!(state,(0,1,"保留的既有摘要".into()));
        assert_eq!(db.query_row("SELECT count(*) FROM ai_memories",[],|r|r.get::<_,i64>(0))?,1);
        assert_eq!(db.query_row("SELECT id FROM ai_memories",[],|r|r.get::<_,i64>(0))?,memory_id);
        assert_eq!(db.query_row("SELECT count(*) FROM ai_memory_provenance",[],|r|r.get::<_,i64>(0))?,1);
        assert_eq!(db.query_row("SELECT count(*) FROM ai_memory_search",[],|r|r.get::<_,i64>(0))?,1);
        Ok(())
    }).await.unwrap();
    chat.shutdown().await;
    host.store.stop().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn facts_only_empty_result_advances_memory_cursor_without_erasing_summary_or_retrying() {
    let (host, chat, model, server) = setup(
        |_| {},
        |batch| {
            assert_eq!(batch["summary_enabled"], false);
            assert_eq!(batch["memory_write"], true);
            assert_eq!(batch["previous_summary"], "");
            assert!(batch.get("expected_summary").is_none());
            let sources = batch["sources"].as_array().unwrap();
            assert_eq!(sources.len(), 1);
            assert_eq!(sources[0]["summary_eligible"], false);
            assert_eq!(sources[0]["memory_eligible"], true);
            fenced(json!({"summary":"","changes":[]}))
        },
    )
    .await;
    host.store
        .call(|db| {
            seed(db, "empty-facts", 1, true, "{}", 1)?;
            db.execute(
                "UPDATE ai_conversations SET summary='已有摘要必须完整保留' WHERE id='empty-facts'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    chat.compact_idle().await.unwrap();
    host.store
        .call(|db| {
            let state: (i64, i64, String) = db.query_row(
                "SELECT compacted_until,memory_processed_until,summary FROM ai_conversations WHERE id='empty-facts'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            assert_eq!(state, (0, 1, "已有摘要必须完整保留".into()));
            for table in ["ai_memories", "ai_memory_provenance", "ai_memory_search"] {
                assert_eq!(db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get::<_,i64>(0))?, 0);
            }
            assert_eq!(db.query_row("SELECT COUNT(*) FROM messages WHERE conversation_id='empty-facts' AND revoked=0", [], |row| row.get::<_,i64>(0))?, 1);
            Ok(())
        })
        .await
        .unwrap();
    chat.compact_idle().await.unwrap();
    assert_eq!(
        model.inputs.lock().unwrap().len(),
        1,
        "a valid empty result is completed work, not a rejected batch to retry"
    );
    chat.shutdown().await;
    host.store.stop().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn summary_only_requests_disable_fact_mutations_and_preserve_existing_manual_memory() {
    const MEMORY_TEXT: &str = "管理员保留的完整记忆：用户喜欢白茶，原文不得被后台摘要改写。";
    let (host, chat, model, server) = setup(
        |config| {
            config.plugins.ai.defaults.memory_write = false;
            config.plugins.ai.defaults.recent_messages = 2;
        },
        |batch| {
            assert_eq!(batch["summary_enabled"], true);
            assert_eq!(batch["memory_write"], false);
            assert_eq!(batch["previous_summary"], "原有会话摘要");
            assert_eq!(batch["allowed_memories"], json!([]));
            assert!(batch.get("expected_summary").is_none());
            let sources = batch["sources"].as_array().unwrap();
            assert_eq!(sources.len(), 1);
            assert_eq!(sources[0]["summary_eligible"], true);
            assert_eq!(sources[0]["memory_eligible"], false);
            assert_eq!(batch["through_position"], 1);
            fenced(json!({"summary":"用户7谈过喝茶偏好。","changes":[]}))
        },
    )
    .await;
    host.store
        .call(|db| {
            for position in 1..=3 {
                seed(db, "summary-only", 1, true, "{}", position)?;
            }
            db.execute(
                "UPDATE ai_conversations SET summary='原有会话摘要' WHERE id='summary-only'",
                [],
            )?;
            db.execute(
                "INSERT INTO ai_memories(conversation_id,subject_id,kind,text,evidence,created_at,updated_at,origin,active,locked,version,edited_by) VALUES('summary-only',7,'preference',?1,'[]',1,1,'manual',1,1,3,'synthetic-admin')",
                [MEMORY_TEXT],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    chat.compact_idle().await.unwrap();
    host.store
        .call(|db| {
            let state: (i64, i64, String, i64) = db.query_row(
                "SELECT compacted_until,memory_processed_until,summary,memory_revision FROM ai_conversations WHERE id='summary-only'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            assert_eq!(state, (1, 0, "用户7谈过喝茶偏好。".into(), 0));
            let saved: (String, String, bool, i64, String, String, i64) = db.query_row(
                "SELECT text,origin,locked,version,edited_by,evidence,updated_at FROM ai_memories WHERE conversation_id='summary-only'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
            )?;
            assert_eq!(saved, (MEMORY_TEXT.into(), "manual".into(), true, 3, "synthetic-admin".into(), "[]".into(), 1));
            assert_eq!(db.query_row("SELECT COUNT(*) FROM ai_memories", [], |row| row.get::<_,i64>(0))?, 1);
            assert_eq!(db.query_row("SELECT COUNT(*) FROM messages WHERE conversation_id='summary-only' AND revoked=0", [], |row| row.get::<_,i64>(0))?, 3);
            Ok(())
        })
        .await
        .unwrap();
    chat.compact_idle().await.unwrap();
    assert_eq!(model.inputs.lock().unwrap().len(), 1);
    chat.shutdown().await;
    host.store.stop().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn fenced_invalid_evidence_rejects_the_whole_batch_without_advancing_cursors() {
    let (host, chat, model, server) = setup(
        |_| {},
        |batch| {
            fenced(json!({"summary":"","changes":[
                {"op":"upsert","id":null,"subject_id":7,"kind":"preference","text":"喜欢白茶","evidence":[{"source_key":batch["sources"][0]["key"],"quote":"喜欢白茶"}],"ttl_days":null},
                {"op":"upsert","id":null,"subject_id":7,"kind":"preference","text":"喜欢咖啡","evidence":[{"source_key":batch["sources"][0]["key"],"quote":"喜欢咖啡"}],"ttl_days":null}
            ]}))
        },
    )
    .await;
    host.store
        .call(|db| {
            seed(db, "invalid-evidence", 1, true, "{}", 1)?;
            db.execute(
                "UPDATE ai_conversations SET summary='原摘要' WHERE id='invalid-evidence'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    chat.compact_idle().await.unwrap();
    assert_eq!(model.inputs.lock().unwrap().len(), 1);
    host.store.call(|db| {
        let state:(i64,i64,String)=db.query_row("SELECT compacted_until,memory_processed_until,summary FROM ai_conversations WHERE id='invalid-evidence'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        assert_eq!(state,(0,0,"原摘要".into()));
        for table in ["ai_memories","ai_memory_provenance","ai_memory_search"] {
            assert_eq!(db.query_row(&format!("SELECT count(*) FROM {table}"),[],|r|r.get::<_,i64>(0))?,0,"{table}");
        }
        assert_eq!(db.query_row("SELECT count(*) FROM messages WHERE conversation_id='invalid-evidence' AND revoked=0",[],|r|r.get::<_,i64>(0))?,1);
        Ok(())
    }).await.unwrap();
    chat.compact_idle().await.unwrap();
    assert_eq!(
        model.inputs.lock().unwrap().len(),
        1,
        "invalid evidence remains in retry backoff"
    );
    chat.shutdown().await;
    host.store.stop().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn malformed_model_results_do_not_starve_the_ninth_conversation() {
    let (host, chat, model, server) = setup(
        |_| {},
        |batch| {
            let id = batch["guard"]["conversation_id"].as_str().unwrap();
            if id < "chat-08" {
                "invalid model JSON".into()
            } else {
                json!({"summary":"","changes":[]}).to_string()
            }
        },
    )
    .await;
    host.store
        .call(|db| {
            for i in 0..10 {
                seed(db, &format!("chat-{i:02}"), 1, true, "{}", 1)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    chat.compact_idle().await.unwrap();
    assert_eq!(model.inputs.lock().unwrap().len(), 8);
    chat.compact_idle().await.unwrap();
    assert_eq!(
        model.inputs.lock().unwrap()[8]["guard"]["conversation_id"],
        "chat-08"
    );
    chat.compact_idle().await.unwrap();
    assert_eq!(
        model.inputs.lock().unwrap()[9]["guard"]["conversation_id"],
        "chat-09"
    );
    chat.compact_idle().await.unwrap();
    assert_eq!(
        model.inputs.lock().unwrap().len(),
        10,
        "failed conversations must remain in backoff"
    );
    host.store
        .call(|db| {
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM ai_conversations WHERE memory_processed_until=1",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                2
            );
            Ok(())
        })
        .await
        .unwrap();
    chat.shutdown().await;
    host.store.stop().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn oversized_old_summary_is_repaired_without_dropping_any_input_chunk() {
    let (host, chat, model, server) = setup(
        |config| {
            config.plugins.ai.memory_input_budget = 3072;
            config.plugins.ai.defaults.recent_messages = 2;
            config.plugins.ai.defaults.memory_write = false;
        },
        |batch| {
            let summary = if batch.get("next_part").is_some() {
                "用户7的旧事实"
            } else {
                "用户7的旧事实和新话题"
            };
            fenced(json!({"summary":summary,"changes":[]}))
        },
    )
    .await;
    let original = format!("{}尾", "甲".repeat(1499));
    let old = original.clone();
    host.store.call(move |db| {
        for position in 2..=4 {seed(db,"old",1,true,"{}",position)?;}
        db.execute("UPDATE ai_conversations SET summary=?1,compacted_until=1,memory_processed_until=1 WHERE id='old'",[old])?;
        Ok(())
    }).await.unwrap();
    chat.compact_idle().await.unwrap();
    let chunks = model.inputs.lock().unwrap().clone();
    assert!(chunks.len() > 1);
    assert!(chunks.len() <= 32);
    assert_eq!(
        chunks
            .iter()
            .map(|input| input["next_part"].as_str().unwrap())
            .collect::<String>(),
        original
    );
    host.store.call(|db| {
        let state:(String,i64,i64)=db.query_row("SELECT summary,compacted_until,memory_processed_until FROM ai_conversations WHERE id='old'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        assert_eq!(state,("用户7的旧事实".into(),1,1));
        Ok(())
    }).await.unwrap();
    // The repaired value can actually be used by the following normal batch.
    chat.compact_idle().await.unwrap();
    assert!(
        model
            .inputs
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .get("guard")
            .is_some()
    );
    host.store
        .call(|db| {
            assert_eq!(
                db.query_row(
                    "SELECT compacted_until FROM ai_conversations WHERE id='old'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                2
            );
            Ok(())
        })
        .await
        .unwrap();
    chat.shutdown().await;
    host.store.stop().await.unwrap();
    server.abort();
}

const PENDING_REPAIR_OLD_SUMMARY: &str = "已有摘要：用户7喜欢白茶。";
const PENDING_REPAIR_FINAL_SUMMARY: &str = "用户7喜欢白茶；保留已有喝茶偏好。";

fn pending_repair_candidate() -> String {
    // Within the 1,500-character ceiling but over the default 1,050-byte budget.
    "用户7喜欢白茶。".repeat(60)
}

fn pending_repair_response(batch: &serde_json::Value, invalid_evidence: bool) -> String {
    assert_eq!(batch["summary_enabled"], true);
    assert_eq!(batch["memory_write"], true);
    assert_eq!(batch["previous_summary"], PENDING_REPAIR_OLD_SUMMARY);
    let candidate = pending_repair_candidate();
    assert!(
        qkbot::plugins::ai::memory::summary_cost(&candidate).unwrap()
            > batch["summary_budget"].as_u64().unwrap() as usize
    );
    fenced(serde_json::json!({
        "summary":candidate,
        "changes":[{
            "op":"upsert", "id":null, "subject_id":7,
            "kind":"preference", "text":"喜欢白茶",
            "evidence":[{
                "source_key":batch["sources"][0]["key"],
                "quote":if invalid_evidence { "喜欢咖啡" } else { "喜欢白茶" }
            }], "ttl_days":null
        }]
    }))
}

async fn seed_pending_repair(host: &qkbot::host::Host) {
    host.store
        .call(|db| {
            for position in 1..=3 {
                seed(db, "pending-repair", 1, true, "{}", position)?;
            }
            db.execute(
                "UPDATE ai_conversations SET summary=?1 WHERE id='pending-repair'",
                [PENDING_REPAIR_OLD_SUMMARY],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn assert_pending_repair_not_committed(host: &qkbot::host::Host, expected_revision: i64) {
    host.store.call(move |db| {
        let state: (String, i64, i64, i64) = db.query_row(
            "SELECT summary,compacted_until,memory_processed_until,memory_revision FROM ai_conversations WHERE id='pending-repair'",
            [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
        )?;
        assert_eq!(state, (PENDING_REPAIR_OLD_SUMMARY.into(), 0, 0, expected_revision));
        for table in ["ai_memories", "ai_memory_provenance", "ai_memory_search"] {
            assert_eq!(db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r|r.get::<_,i64>(0))?, 0);
        }
        assert_eq!(db.query_row("SELECT COUNT(*) FROM messages WHERE conversation_id='pending-repair' AND revoked=0", [], |r|r.get::<_,i64>(0))?, 3);
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn oversized_new_summary_is_compressed_then_committed_with_original_valid_facts() {
    let (host, chat, model, server) = setup(
        |config| config.plugins.ai.defaults.recent_messages = 2,
        |input| {
            if input.get("sources").is_some() {
                pending_repair_response(input, false)
            } else {
                assert_eq!(input["previous_summary"], "");
                assert_eq!(input["next_part"], pending_repair_candidate());
                assert!(input.get("changes").is_none());
                assert!(input.get("allowed_memories").is_none());
                fenced(serde_json::json!({"summary":PENDING_REPAIR_FINAL_SUMMARY,"changes":[]}))
            }
        },
    )
    .await;
    seed_pending_repair(&host).await;
    let barrier = Arc::new(RepairBarrier::default());
    *model.repair_barrier.lock().unwrap() = Some(barrier.clone());
    let pending_chat = chat.clone();
    let work = tokio::spawn(async move { pending_chat.compact_idle().await });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        barrier.entered.notified(),
    )
    .await
    .expect("pending summary repair never reached the mock");
    assert_pending_repair_not_committed(&host, 0).await;
    barrier.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), work)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(model.inputs.lock().unwrap().len(), 2);
    host.store.call(|db| {
        let state: (String,i64,i64) = db.query_row(
            "SELECT summary,compacted_until,memory_processed_until FROM ai_conversations WHERE id='pending-repair'",
            [], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        )?;
        assert_eq!(state,(PENDING_REPAIR_FINAL_SUMMARY.into(),1,1));
        let fact: (i64,String,String) = db.query_row(
            "SELECT subject_id,text,evidence FROM ai_memories WHERE conversation_id='pending-repair'",
            [], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        )?;
        assert_eq!((fact.0,fact.1.as_str()),(7,"喜欢白茶"));
        assert_eq!(serde_json::from_str::<serde_json::Value>(&fact.2)?,serde_json::json!([{"source_key":"pending-repair:1","quote":"喜欢白茶"}]));
        for table in ["ai_memories", "ai_memory_provenance", "ai_memory_search"] {
            assert_eq!(db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r|r.get::<_,i64>(0))?,1);
        }
        Ok(())
    }).await.unwrap();
    chat.shutdown().await;
    host.store.stop().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn invalid_pending_summary_compression_cannot_partially_commit_facts_or_cursors() {
    for failure in [
        "extra_changes",
        "still_oversized",
        "empty_summary",
        "invalid_json",
    ] {
        let (host, chat, model, server) = setup(
            |config| config.plugins.ai.defaults.recent_messages = 2,
            move |input| {
                if input.get("sources").is_some() {
                    return pending_repair_response(input, false);
                }
                match failure {
                    "extra_changes" => fenced(serde_json::json!({"summary":PENDING_REPAIR_FINAL_SUMMARY,"changes":[{
                        "op":"upsert", "id":null, "subject_id":7, "kind":"preference", "text":"压缩模型另造事实",
                        "evidence":[{"source_key":"pending-repair:1","quote":"喜欢白茶"}], "ttl_days":null
                    }]})),
                    "still_oversized" => fenced(serde_json::json!({"summary":pending_repair_candidate(),"changes":[]})),
                    "empty_summary" => fenced(serde_json::json!({"summary":"","changes":[]})),
                    "invalid_json" => "invalid JSON".into(),
                    _ => unreachable!(),
                }
            },
        ).await;
        seed_pending_repair(&host).await;
        chat.compact_idle().await.unwrap();
        assert_pending_repair_not_committed(&host, 0).await;
        let attempts = model.inputs.lock().unwrap().len();
        assert_eq!(
            attempts, 2,
            "invalid compression output must not be retried: {failure}"
        );
        chat.compact_idle().await.unwrap();
        assert_eq!(model.inputs.lock().unwrap().len(), attempts);
        chat.shutdown().await;
        host.store.stop().await.unwrap();
        server.abort();
    }
}

#[tokio::test]
async fn compressed_summary_never_authorizes_an_original_invalid_evidence_quote() {
    let (host, chat, _model, server) = setup(
        |config| config.plugins.ai.defaults.recent_messages = 2,
        |input| {
            if input.get("sources").is_some() {
                pending_repair_response(input, true)
            } else {
                fenced(serde_json::json!({"summary":PENDING_REPAIR_FINAL_SUMMARY,"changes":[]}))
            }
        },
    )
    .await;
    seed_pending_repair(&host).await;
    chat.compact_idle().await.unwrap();
    assert_pending_repair_not_committed(&host, 0).await;
    chat.shutdown().await;
    host.store.stop().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn pending_summary_compression_cancel_or_changed_revision_cannot_commit_the_original_batch() {
    for change in ["cancel", "revision", "epoch"] {
        let (host, chat, model, server) = setup(
            |config| config.plugins.ai.defaults.recent_messages = 2,
            |input| {
                if input.get("sources").is_some() {
                    pending_repair_response(input, false)
                } else {
                    fenced(json!({"summary":PENDING_REPAIR_FINAL_SUMMARY,"changes":[]}))
                }
            },
        )
        .await;
        seed_pending_repair(&host).await;
        let barrier = Arc::new(RepairBarrier::default());
        *model.repair_barrier.lock().unwrap() = Some(barrier.clone());
        let pending_chat = chat.clone();
        let work = tokio::spawn(async move { pending_chat.compact_idle().await });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            barrier.entered.notified(),
        )
        .await
        .expect("pending summary repair never reached the mock");
        assert_pending_repair_not_committed(&host, 0).await;
        match change {
            "cancel" => chat.cancel(&ContextGuard { conversation_id:"pending-repair".into(), epoch:1 }),
            "revision" => host.store.call(|db| {
                db.execute("UPDATE ai_conversations SET memory_revision=memory_revision+1 WHERE id='pending-repair'", [])?;
                Ok(())
            }).await.unwrap(),
            "epoch" => host.store.call(|db| {
                db.execute("UPDATE ai_conversations SET epoch=epoch+1 WHERE id='pending-repair'", [])?;
                Ok(())
            }).await.unwrap(),
            _ => unreachable!(),
        }
        barrier.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), work)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_pending_repair_not_committed(&host, i64::from(change == "revision")).await;
        let epoch: i64 = host
            .store
            .call(|db| {
                Ok(db.query_row(
                    "SELECT epoch FROM ai_conversations WHERE id='pending-repair'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(epoch, if change == "epoch" { 2 } else { 1 });
        chat.shutdown().await;
        host.store.stop().await.unwrap();
        server.abort();
    }
}

#[tokio::test]
async fn pending_summary_compression_enforces_candidate_size_and_two_chunk_limit() {
    for size in [7000, 8193] {
        let candidate = "甲".repeat(size);
        let (host, chat, model, server) = setup(
            |config| config.plugins.ai.defaults.recent_messages = 2,
            move |input| {
                if input.get("sources").is_some() {
                    let mut response: Value = serde_json::from_str(
                        pending_repair_response(input, false)
                            .strip_prefix("```json\n")
                            .unwrap()
                            .strip_suffix("\n```")
                            .unwrap(),
                    )
                    .unwrap();
                    response["summary"] = json!(candidate);
                    fenced(response)
                } else {
                    fenced(json!({"summary":PENDING_REPAIR_FINAL_SUMMARY,"changes":[]}))
                }
            },
        )
        .await;
        seed_pending_repair(&host).await;
        chat.compact_idle().await.unwrap();
        assert_pending_repair_not_committed(&host, 0).await;
        let inputs = model.inputs.lock().unwrap().clone();
        assert_eq!(inputs.len(), if size == 8193 { 1 } else { 3 });
        if size == 7000 {
            let consumed = inputs
                .iter()
                .skip(1)
                .map(|input| input["next_part"].as_str().unwrap().len())
                .sum::<usize>();
            assert!(
                consumed < size * 3,
                "the remaining candidate must not be silently discarded"
            );
        }
        chat.shutdown().await;
        host.store.stop().await.unwrap();
        server.abort();
    }
}
