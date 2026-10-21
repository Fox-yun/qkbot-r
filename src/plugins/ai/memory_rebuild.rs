//! Explicit, resumable history learning. Does not start Host or recover its outbox.
use super::{
    chat::memory_system_prompt,
    memory::{self, BatchUpdate, ContextInput, rebuild},
};
use crate::{config::Config, llm::Llm, milky::Milky, types::now};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const INPUT_BUDGET: usize = 32_768;
const MAX_SOURCES: usize = 48;

fn database_path(path: &Path, config: &Config) -> PathBuf {
    let database = Path::new(&config.app.database);
    if database.is_absolute() {
        database.to_owned()
    } else {
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(database)
    }
}

fn open(path: &Path, config: &Config, readonly: bool) -> Result<Connection> {
    let flags = if readonly {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let db = Connection::open_with_flags(database_path(path, config), flags)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch("PRAGMA foreign_keys=ON;")?;
    Ok(db)
}

fn emit(event: &str, status: impl serde::Serialize) -> Result<()> {
    let mut status = serde_json::to_value(status)?;
    // Operational logs contain counts, never account IDs, messages or model text.
    if let Some(object) = status.as_object_mut() {
        object.remove("bot_id");
    }
    println!("{}", json!({"event": event, "status": status}));
    Ok(())
}

fn config_digest(path: &Path) -> Result<Vec<u8>> {
    Ok(Sha256::digest(std::fs::read(path)?).to_vec())
}

fn concurrent_change(error: &anyhow::Error) -> bool {
    matches!(
        error.to_string().as_str(),
        "memories changed while rebuilding"
            | "existing rebuild references changed"
            | "rebuild checkpoint changed"
    )
}

fn reserve(db: &mut Connection, config: &Config, batch: &rebuild::Batch) -> Result<()> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // No await between this snapshot/admission check and the model request.
    rebuild::validate_snapshot(&tx, batch, &config.plugins.ai.defaults)?;
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM model_admissions WHERE purpose='background' AND at>?1",
        [now() - 3600],
        |row| row.get(0),
    )?;
    ensure!(
        count < config.limits.memory_requests_per_hour as i64,
        "memory_rebuild_request_quota"
    );
    tx.execute(
        "INSERT INTO model_admissions(at,purpose,actor,conversation_id) VALUES(?1,'background','',?2)",
        params![now(), batch.guard.conversation_id],
    )?;
    tx.commit()?;
    Ok(())
}

fn record_usage(db: &Connection, response: &crate::llm::LlmResponse) -> Result<()> {
    db.execute(
        "INSERT INTO usage_daily(day,plugin_id,purpose,model,requests,input_tokens,output_tokens) VALUES(date('now'),'ai','memory_rebuild',?1,1,?2,?3) ON CONFLICT(day,plugin_id,purpose,model) DO UPDATE SET requests=requests+1,input_tokens=input_tokens+excluded.input_tokens,output_tokens=output_tokens+excluded.output_tokens",
        params![response.model, i64::try_from(response.usage.input_tokens)?, i64::try_from(response.usage.output_tokens)?],
    )?;
    Ok(())
}

fn system(config: &Config) -> String {
    format!(
        "{} 本次是历史补整理：summary必须为空。existing_memories是当前人物全部既有记忆的只读参考，包括停用和过期记录；同一事项即使措辞不同也不要重复，已覆盖的内容省略，不能修改旧记录或恢复已过期/停用内容。新增id=null。人物画像仍以本批同一人的原文为证据。历史中的时间、计划与完成状态须保留，不能把旧计划写成当前计划，不因回扫而延长临时状态。每个不同事项只新增一项，合并同人的重复表达；允许changes为空。",
        memory_system_prompt(128, &config.app.timezone)
    )
}

fn input(batch: &rebuild::Batch, config: &Config) -> Result<Vec<Value>> {
    Ok(memory::build_context(
        ContextInput {
            system: system(config),
            current: serde_json::to_string(&batch.input())?,
            history: vec![],
            memories: vec![],
            summary: String::new(),
            tool_schema: json!([]),
            tool_protocol: vec![],
        },
        INPUT_BUDGET,
    )?
    .messages)
}

pub fn show_status(path: &Path, run_id: &str) -> Result<()> {
    let config = Config::load(path)?;
    let db = open(path, &config, true)?;
    emit("memory_rebuild_status", rebuild::status(&db, run_id)?)
}

/// A bounded invocation resumes the same frozen run ID. Every successful batch
/// commits its receipt and new memories atomically, so interruption is safe.
pub async fn run(path: &Path, run_id: &str, max_batches: usize) -> Result<()> {
    let config = Config::load(path).context("rebuild_config")?;
    ensure!(config.plugins.ai.enabled, "memory_rebuild_ai_disabled");
    ensure!(max_batches <= 10_000, "memory_rebuild_batch_limit");
    let digest = config_digest(path)?;
    let mut db = open(path, &config, false).context("rebuild_database")?;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()?;
    let milky = Milky::new(config.milky.clone(), http.clone())?;
    let bot_id = milky.login().await.context("rebuild_verify_account")?;
    let initial = rebuild::start(&mut db, run_id, bot_id, &config.plugins.ai.defaults)
        .context("rebuild_start")?;
    emit("memory_rebuild_started", initial)?;
    let llm = Llm::new(config.llm.clone(), http)?;
    let cancel = CancellationToken::new();
    for _ in 0..max_batches {
        ensure!(
            config_digest(path)? == digest,
            "memory_rebuild_configuration_changed"
        );
        let mut max_sources = MAX_SOURCES;
        let mut collisions = 0;
        loop {
            let Some(batch) =
                rebuild::prepare(&db, run_id, &config.plugins.ai.defaults, max_sources)
                    .context("rebuild_prepare")?
            else {
                return emit("memory_rebuild_complete", rebuild::status(&db, run_id)?);
            };
            let update = if batch.sources.is_empty() {
                BatchUpdate {
                    summary: String::new(),
                    changes: vec![],
                }
            } else {
                let messages = match input(&batch, &config) {
                    Ok(messages) => messages,
                    Err(_) if max_sources > 1 => {
                        max_sources /= 2;
                        continue;
                    }
                    Err(_) => anyhow::bail!("memory_rebuild_full_reference_exceeds_budget"),
                };
                milky
                    .verify_account(bot_id)
                    .await
                    .context("rebuild_verify_account")?;
                ensure!(
                    config_digest(path)? == digest,
                    "memory_rebuild_configuration_changed"
                );
                if let Err(error) = reserve(&mut db, &config, &batch) {
                    if collisions < 3 && concurrent_change(&error) {
                        collisions += 1;
                        emit(
                            "memory_rebuild_retry",
                            json!({"reason":"concurrent_memory_change","attempt":collisions}),
                        )?;
                        continue;
                    }
                    return Err(error.context("rebuild_admission"));
                }
                let response = llm
                    .complete(
                        messages,
                        vec![],
                        config.plugins.ai.memory_output_budget,
                        &cancel,
                    )
                    .await
                    .context("rebuild_model_request")?;
                record_usage(&db, &response).context("rebuild_usage")?;
                let update = memory::parse_batch_update(
                    response.message["content"]
                        .as_str()
                        .context("rebuild_missing_response")?,
                )
                .context("rebuild_response_format")?;
                // Do not silently consume a large source window when the model
                // has filled every available change slot.
                if update.changes.len() == 16 && batch.sources.len() > 16 {
                    max_sources = 16;
                    continue;
                }
                update
            };
            ensure!(
                config_digest(path)? == digest,
                "memory_rebuild_configuration_changed"
            );
            milky
                .verify_account(bot_id)
                .await
                .context("rebuild_verify_account")?;
            let committed = match rebuild::commit(
                &mut db,
                &batch,
                update,
                &config.plugins.ai.defaults,
                config.plugins.ai.max_memories_per_conversation,
            ) {
                Ok(committed) => committed,
                Err(error) if collisions < 3 && concurrent_change(&error) => {
                    collisions += 1;
                    emit(
                        "memory_rebuild_retry",
                        json!({"reason":"concurrent_memory_change","attempt":collisions}),
                    )?;
                    continue;
                }
                Err(error) => return Err(error.context("rebuild_commit")),
            };
            emit("memory_rebuild_batch", committed)?;
            break;
        }
    }
    emit("memory_rebuild_checkpoint", rebuild::status(&db, run_id)?)
}

pub fn failure_diagnostic(error: &anyhow::Error) -> Value {
    let outer = error.to_string();
    let stage = match outer.as_str() {
        "rebuild_config"
        | "rebuild_database"
        | "rebuild_verify_account"
        | "rebuild_start"
        | "rebuild_prepare"
        | "rebuild_admission"
        | "rebuild_model_request"
        | "rebuild_usage"
        | "rebuild_missing_response"
        | "rebuild_response_format"
        | "rebuild_commit"
        | "memory_rebuild_ai_disabled"
        | "memory_rebuild_batch_limit"
        | "memory_rebuild_configuration_changed"
        | "memory_rebuild_full_reference_exceeds_budget"
        | "memory_rebuild_request_quota" => outer.as_str(),
        _ => "rebuild_failure",
    };
    let mut reason = "validation_or_storage";
    for cause in error.chain() {
        reason = match cause.to_string().as_str() {
            "memory_rebuild_request_quota" => "request_quota",
            "rebuild memory capacity exceeded; checkpoint was not advanced" => "capacity",
            "memories changed while rebuilding"
            | "existing rebuild references changed"
            | "rebuild checkpoint changed" => "concurrent_change",
            "rebuild evidence must quote its human subject exactly"
            | "rebuild evidence was not in this batch" => "evidence",
            "chat observations need two distinct human messages" => "profile_evidence",
            "chat observations need a 7..30 day TTL" => "profile_ttl",
            _ => continue,
        };
        break;
    }
    json!({"event":"memory_rebuild_failed","stage":stage,"reason":reason})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_relative_database_beside_config_without_opening_runtime() {
        let mut config = Config::default();
        config.app.database = "history.sqlite3".into();
        assert_eq!(
            database_path(Path::new("fixture/config.toml"), &config),
            PathBuf::from("fixture/history.sqlite3")
        );
    }
    #[test]
    fn failure_metadata_never_echoes_unknown_error_or_provider_text() {
        let error = anyhow::anyhow!("private-message-or-key").context("rebuild_commit");
        let safe = failure_diagnostic(&error).to_string();
        assert!(safe.contains("rebuild_commit"));
        assert!(!safe.contains("private-message-or-key"));
        assert!(
            !failure_diagnostic(&anyhow::anyhow!("private-key"))
                .to_string()
                .contains("private-key")
        );
    }
}
