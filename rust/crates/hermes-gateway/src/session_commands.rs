//! Stateful native session commands.

use std::sync::atomic::Ordering;
use std::sync::Arc;

pub struct ResetResult {
    pub reply: String,
}

pub struct ResumeResult {
    pub reply: String,
}

pub struct ResumeCommand<'a> {
    pub confirmations: &'a crate::slash_confirm::SlashConfirmations,
    pub deps: crate::session_admission::AdmissionDeps,
    pub agent: Arc<dyn crate::agent::AgentClient>,
    pub source: crate::session::SessionSource,
    pub message: &'a hermes_core::Message,
    pub user_config: &'a serde_json::Value,
    pub raw_args: &'a str,
    pub from_sessions: bool,
}

struct ResumeRequest {
    target: Option<String>,
    allow_all: bool,
    allow_cross_room: bool,
    include_unnamed: bool,
    search_query: Option<String>,
}

struct ResumeListQuery {
    store: Arc<crate::session_store::SessionStore>,
    database: Arc<crate::session_db::SessionDb>,
    source: crate::session::SessionSource,
    source_filter: Option<String>,
    lane: Option<String>,
    include_unnamed: bool,
    allow_override: bool,
}

fn parse_resume_request(raw: &str, from_sessions: bool) -> anyhow::Result<ResumeRequest> {
    let parts = shell_words::split(raw)?;
    let (allow_all, allow_cross_room, include_unnamed, search_query, target_parts) =
        if from_sessions {
            let mut allow_all = false;
            let mut allow_cross_room = false;
            let mut include_unnamed = false;
            let mut target_parts = Vec::new();
            let mut search_query = None;
            let mut index = 0;
            while index < parts.len() {
                let lower = parts[index].trim().to_ascii_lowercase();
                if lower == "--cross-room" {
                    allow_cross_room = true;
                    index += 1;
                    continue;
                }
                if target_parts.is_empty() {
                    match lower.as_str() {
                        "list" | "ls" | "browse" => {
                            index += 1;
                            continue;
                        }
                        "all" | "--all" => {
                            allow_all = true;
                            index += 1;
                            continue;
                        }
                        "full" | "--full" => {
                            include_unnamed = true;
                            index += 1;
                            continue;
                        }
                        "search" | "find" => {
                            search_query = Some(parts[index + 1..].join(" ").trim().to_owned());
                            break;
                        }
                        _ => {}
                    }
                }
                target_parts.push(parts[index].clone());
                index += 1;
            }
            (
                allow_all,
                allow_cross_room,
                include_unnamed,
                search_query,
                target_parts,
            )
        } else {
            let allow_all = parts.iter().any(|part| part == "--all");
            let allow_cross_room = parts.iter().any(|part| part == "--cross-room");
            let target_parts = parts
                .into_iter()
                .filter(|part| part != "--all" && part != "--cross-room")
                .collect();
            (allow_all, allow_cross_room, false, None, target_parts)
        };
    let mut target = target_parts.join(" ").trim().to_owned();
    if target.len() >= 2 {
        let first = target.as_bytes()[0];
        let last = target.as_bytes()[target.len() - 1];
        if matches!(
            (first, last),
            (b'<', b'>') | (b'[', b']') | (b'"', b'"') | (b'\'', b'\'')
        ) {
            target = target[1..target.len() - 1].trim().to_owned();
        }
    }
    Ok(ResumeRequest {
        target: (!target.is_empty()).then_some(target),
        allow_all,
        allow_cross_room,
        include_unnamed,
        search_query,
    })
}

fn resume_target_allowed(
    store: &crate::session_store::SessionStore,
    database: &crate::session_db::SessionDb,
    source: &crate::session::SessionSource,
    target_id: &str,
    explicit_admin_override: bool,
) -> anyhow::Result<bool> {
    if explicit_admin_override {
        return Ok(true);
    }
    let route_key = store.session_key_for_source(source);
    let dm_user_matches = |target_user: Option<&str>| {
        source.chat_type != "dm"
            || source
                .user_id
                .as_deref()
                .is_some_and(|user| target_user == Some(user))
    };
    if let Some(entry) = store.lookup_by_session_id(target_id) {
        if entry.session_key == route_key
            && entry.origin.as_ref().is_some_and(|origin| {
                origin.platform == source.platform
                    && origin.chat_id == source.chat_id
                    && origin.thread_id.as_deref().unwrap_or("")
                        == source.thread_id.as_deref().unwrap_or("")
                    && dm_user_matches(origin.user_id.as_deref())
            })
        {
            return Ok(true);
        }
    }
    let Some(row) = database.get_session(target_id)? else {
        return Ok(false);
    };
    Ok(row["session_key"].as_str() == Some(route_key.as_str())
        && row["source"].as_str() == Some(source.platform.as_str())
        && row["chat_id"].as_str() == Some(source.chat_id.as_str())
        && row["thread_id"].as_str().unwrap_or("") == source.thread_id.as_deref().unwrap_or("")
        && dm_user_matches(row["user_id"].as_str()))
}

fn query_visible_resume_sessions(
    query: ResumeListQuery,
    limit: usize,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let rows = query.database.list_resume_sessions(
        query.source_filter.as_deref(),
        query.lane.as_deref(),
        query.include_unnamed,
        limit.saturating_mul(4).max(limit),
    )?;
    let mut visible = Vec::with_capacity(limit);
    for row in rows {
        let Some(id) = row["id"].as_str() else {
            continue;
        };
        if resume_target_allowed(
            &query.store,
            &query.database,
            &query.source,
            id,
            query.allow_override,
        )? {
            visible.push(row);
            if visible.len() == limit {
                break;
            }
        }
    }
    Ok(visible)
}

pub async fn resume_session(command: ResumeCommand<'_>) -> anyhow::Result<ResumeResult> {
    let ResumeCommand {
        confirmations,
        deps,
        agent,
        source,
        message,
        user_config,
        raw_args,
        from_sessions,
    } = command;
    let request = match parse_resume_request(raw_args, from_sessions) {
        Ok(request) => request,
        Err(error) => {
            return Ok(ResumeResult {
                reply: format!("⚠️ Could not parse `/resume` arguments: {error}.\nUse quotes around titles with spaces, for example: `/resume \"Project A Plan\"`."),
            });
        }
    };
    if request.search_query.as_deref() == Some("") {
        return Ok(ResumeResult {
            reply: "Usage: `/sessions search <query>`".into(),
        });
    }
    if request.search_query.is_some() {
        return Ok(ResumeResult {
            reply: "Session search is not available in the native gateway yet.".into(),
        });
    }
    let route_key = deps.store.session_key_for_source(&source);
    let database = deps
        .store
        .database_for_key(&route_key)
        .ok_or_else(|| anyhow::anyhow!("session database is unavailable"))?;
    let explicit_admin = crate::slash::is_explicit_admin(user_config, message);
    let target_override = explicit_admin && (request.allow_all || request.allow_cross_room);
    let list_widened = explicit_admin && request.allow_all;
    let scope_note = (request.allow_all && !explicit_admin).then_some(
        "_Note: `--all` (cross-chat listing) requires a configured admin; showing this chat's sessions only._",
    );

    if request.target.is_none() {
        let db = database.clone();
        let source_name = (!(from_sessions && list_widened)).then_some(source.platform.clone());
        let lane = (!list_widened).then_some(route_key.clone());
        let include_unnamed = request.include_unnamed;
        let list_store = deps.store.clone();
        let list_source = source.clone();
        let rows = tokio::task::spawn_blocking(move || {
            query_visible_resume_sessions(
                ResumeListQuery {
                    store: list_store,
                    database: db,
                    source: list_source,
                    source_filter: source_name,
                    lane,
                    include_unnamed,
                    allow_override: list_widened,
                },
                10,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("session list worker failed: {error}"))??;
        if rows.is_empty() {
            let mut reply = if request.include_unnamed {
                "No sessions found.".to_owned()
            } else {
                "No named sessions found.\nUse `/sessions full` to include unnamed sessions."
                    .to_owned()
            };
            if let Some(note) = scope_note {
                reply.push('\n');
                reply.push_str(note);
            }
            return Ok(ResumeResult { reply });
        }
        let header = if request.include_unnamed {
            "📋 **Sessions**\n"
        } else {
            "📋 **Named Sessions**\n"
        };
        let mut lines = vec![header.to_owned()];
        for (index, row) in rows.iter().enumerate() {
            let title = row["title"].as_str().unwrap_or("Untitled session");
            let id = row["id"].as_str().unwrap_or_default();
            let preview = row["preview"].as_str().unwrap_or("");
            let preview: String = preview.chars().take(40).collect();
            let suffix = if preview.is_empty() {
                String::new()
            } else {
                format!(" - _{preview}_")
            };
            let source_suffix = if from_sessions && list_widened {
                row["source"]
                    .as_str()
                    .map(|source| format!(" `{source}`"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let id_suffix = if from_sessions {
                format!("{source_suffix} - `{id}`")
            } else {
                String::new()
            };
            lines.push(format!("{}. **{title}**{id_suffix}{suffix}", index + 1));
        }
        if let Some(note) = scope_note {
            lines.push(note.to_owned());
        }
        lines.push(
            "\nUsage: `/resume <session name>` or `/resume <number>` (e.g. `/resume 1` for the most recent)".into(),
        );
        return Ok(ResumeResult {
            reply: lines.join("\n"),
        });
    }

    let mut target_name = request.target.unwrap();
    let target_id = if target_name
        .chars()
        .all(|character| character.is_ascii_digit())
    {
        let db = database.clone();
        let source_name = (!(from_sessions && list_widened)).then_some(source.platform.clone());
        let lane = (!list_widened).then_some(route_key.clone());
        let list_store = deps.store.clone();
        let list_source = source.clone();
        let rows = tokio::task::spawn_blocking(move || {
            query_visible_resume_sessions(
                ResumeListQuery {
                    store: list_store,
                    database: db,
                    source: list_source,
                    source_filter: source_name,
                    lane,
                    include_unnamed: false,
                    allow_override: list_widened,
                },
                10,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("session list worker failed: {error}"))??;
        let index = target_name.parse::<usize>().unwrap_or(0);
        let Some(row) = index.checked_sub(1).and_then(|index| rows.get(index)) else {
            return Ok(ResumeResult {
                reply: format!("Resume index {index} is out of range.\nUse `/resume` with no arguments to see available sessions."),
            });
        };
        let id = row["id"].as_str().unwrap_or_default().to_owned();
        if let Some(title) = row["title"].as_str() {
            target_name = title.to_owned();
        }
        id
    } else {
        let db = database.clone();
        let query = target_name.clone();
        let resolved = tokio::task::spawn_blocking(move || db.resolve_session_target(&query))
            .await
            .map_err(|error| anyhow::anyhow!("session target worker failed: {error}"))??;
        let Some(resolved) = resolved else {
            return Ok(ResumeResult {
                reply: format!("No session found matching '**{target_name}**'.\nUse `/resume` with no arguments to see available sessions."),
            });
        };
        resolved
    };
    let db = database.clone();
    let target_id = tokio::task::spawn_blocking(move || db.get_compression_tip(&target_id))
        .await
        .map_err(|error| anyhow::anyhow!("resume continuation worker failed: {error}"))??;
    let allowed_store = deps.store.clone();
    let allowed_db = database.clone();
    let allowed_source = source.clone();
    let allowed_target = target_id.clone();
    let allowed = tokio::task::spawn_blocking(move || {
        resume_target_allowed(
            &allowed_store,
            &allowed_db,
            &allowed_source,
            &allowed_target,
            target_override,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("resume authorization worker failed: {error}"))??;
    if !allowed {
        return Ok(ResumeResult {
            reply: format!("⚠️ /resume blocked: '**{target_name}**' belongs to a different user or chat. You can only resume sessions from this chat."),
        });
    }

    let observe_store = deps.store.clone();
    let observe_source = source.clone();
    let route_exists = tokio::task::spawn_blocking(move || {
        observe_store
            .current_entry_for_source(&observe_source)
            .is_some()
    })
    .await
    .map_err(|error| anyhow::anyhow!("resume route probe failed: {error}"))?;
    if !route_exists {
        // Python materializes a current lane before switching. Preserve that
        // first-command behavior; the empty predecessor becomes a durable
        // session_switch boundary and never reaches the model.
        reset_session(
            deps.clone(),
            agent.clone(),
            source.clone(),
            &message.sender_id,
            None,
        )
        .await?;
    }

    for _ in 0..32 {
        let route_token = deps
            .route_leases
            .acquire(
                &route_key,
                &message.sender_id,
                deps.generation.fetch_add(1, Ordering::Relaxed),
                None,
            )
            .await
            .map_err(|error| anyhow::anyhow!(error))?;
        let store = deps.store.clone();
        let observed_source = source.clone();
        let observed =
            tokio::task::spawn_blocking(move || store.current_entry_for_source(&observed_source))
                .await
                .map_err(|error| anyhow::anyhow!("resume route observer failed: {error}"))?;
        let Some(observed) = observed else {
            drop(route_token);
            continue;
        };
        if observed.session_id == target_id {
            drop(route_token);
            return Ok(ResumeResult {
                reply: format!("📌 Already on session **{target_name}**."),
            });
        }

        let mut ids = vec![observed.session_id.clone(), target_id.clone()];
        ids.sort();
        ids.dedup();
        let mut transcript_tokens = Vec::with_capacity(ids.len());
        for id in &ids {
            if let Some(token) = deps
                .transcript_leases
                .acquire(
                    id,
                    &message.sender_id,
                    deps.generation.fetch_add(1, Ordering::Relaxed),
                    None,
                )
                .await
                .map_err(|error| anyhow::anyhow!(error))?
            {
                transcript_tokens.push(token);
            }
        }
        if !deps.store.route_matches(&observed) {
            drop(transcript_tokens);
            drop(route_token);
            continue;
        }
        let allowed_store = deps.store.clone();
        let allowed_db = database.clone();
        let allowed_source = source.clone();
        let allowed_target = target_id.clone();
        let allowed = tokio::task::spawn_blocking(move || {
            resume_target_allowed(
                &allowed_store,
                &allowed_db,
                &allowed_source,
                &allowed_target,
                target_override,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("resume authorization worker failed: {error}"))??;
        if !allowed {
            drop(transcript_tokens);
            drop(route_token);
            return Ok(ResumeResult {
                reply: format!("⚠️ /resume blocked: '**{target_name}**' belongs to a different user or chat. You can only resume sessions from this chat."),
            });
        }
        let switch_store = deps.store.clone();
        let switch_source = source.clone();
        let switch_expected = observed.clone();
        let switch_target = target_id.clone();
        let switched = tokio::task::spawn_blocking(move || {
            switch_store.switch_session(&switch_source, &switch_expected, &switch_target)
        })
        .await
        .map_err(|error| anyhow::anyhow!("session switch worker failed: {error}"))??;
        if switched.is_none() {
            drop(transcript_tokens);
            drop(route_token);
            continue;
        }
        confirmations.clear(&route_key);
        drop(transcript_tokens);
        drop(route_token);
        let summary_db = database.clone();
        let summary_target = target_id.clone();
        let (stored_title, count) = tokio::task::spawn_blocking(move || {
            Ok::<_, rusqlite::Error>((
                summary_db.get_session_title(&summary_target)?,
                summary_db.user_message_count(&summary_target)?,
            ))
        })
        .await
        .map_err(|error| anyhow::anyhow!("resume summary worker failed: {error}"))??;
        let title = stored_title.unwrap_or(target_name);
        let reply = match count {
            0 => format!("↻ Resumed session **{title}**. Conversation restored."),
            1 => format!("↻ Resumed session **{title}** (1 message). Conversation restored."),
            _ => {
                format!("↻ Resumed session **{title}** ({count} messages). Conversation restored.")
            }
        };
        return Ok(ResumeResult { reply });
    }
    anyhow::bail!("session route kept changing during resume")
}

/// Resolve a pending text confirmation before ordinary slash dispatch. `None`
/// means the message was unrelated and must continue through the normal path.
pub async fn resolve_reset_confirmation(
    confirmations: &crate::slash_confirm::SlashConfirmations,
    deps: crate::session_admission::AdmissionDeps,
    agent: Arc<dyn crate::agent::AgentClient>,
    source: crate::session::SessionSource,
    message: &hermes_core::Message,
    user_config: &serde_json::Value,
    tool_approval_live: bool,
) -> Option<ResetResult> {
    let route_key = deps.store.session_key_for_source(&source);
    let resolution =
        confirmations.resolve_text(&route_key, &message.text, tool_approval_live, |command| {
            crate::slash::can_run_command(user_config, message, command)
        });
    match resolution {
        crate::slash_confirm::Resolution::NotHandled => None,
        crate::slash_confirm::Resolution::Expired => Some(ResetResult {
            reply: String::new(),
        }),
        crate::slash_confirm::Resolution::Cancelled { command } => Some(ResetResult {
            reply: format!("🟡 /{command} cancelled. Conversation unchanged."),
        }),
        crate::slash_confirm::Resolution::Approved(approved) => {
            let persisted = if approved.always {
                match confirmations.persist_opt_out().await {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(%error, session = %route_key, "could not persist destructive slash confirmation opt-out");
                        false
                    }
                }
            } else {
                false
            };
            Some(
                match reset_session(deps, agent, source, &message.sender_id, approved.title).await {
                    Ok(mut result) => {
                        if approved.always {
                            if persisted {
                                result.reply.push_str(
                                "\n\nℹ️ Future /clear, /new, /reset, and /undo will run without confirmation. Re-enable via `approvals.destructive_slash_confirm: true` in config.yaml.",
                            );
                            } else {
                                result.reply.push_str(
                                "\n\n⚠️ Could not save that preference (config.yaml is not writable), so /clear, /new, /reset, and /undo will ask again next time. To silence it permanently, set `approvals.destructive_slash_confirm: false` in config.yaml.",
                            );
                            }
                        }
                        result
                    }
                    Err(error) => {
                        tracing::error!(%error, session = %route_key, "destructive slash confirmation handler failed");
                        ResetResult {
                            reply: format!("❌ Error handling confirmation: {error}"),
                        }
                    }
                },
            )
        }
    }
}

/// Gate a newly authorized reset. The prompt is registered before it is
/// returned; when the live config disables confirmation, reset immediately.
pub async fn reset_or_confirm(
    confirmations: &crate::slash_confirm::SlashConfirmations,
    deps: crate::session_admission::AdmissionDeps,
    agent: Arc<dyn crate::agent::AgentClient>,
    source: crate::session::SessionSource,
    owner_key: &str,
    title: Option<String>,
    typed_prefix: &str,
) -> anyhow::Result<ResetResult> {
    if confirmations.required().await {
        let route_key = deps.store.session_key_for_source(&source);
        return Ok(ResetResult {
            reply: confirmations.register_reset(&route_key, title, typed_prefix),
        });
    }
    reset_session(deps, agent, source, owner_key, title).await
}

/// Rotate one stable route while holding the predecessor turn lease. The
/// compare-and-swap store write closes the no-route race without creating an
/// empty predecessor when `/new` is the conversation's first message.
pub async fn reset_session(
    deps: crate::session_admission::AdmissionDeps,
    agent: Arc<dyn crate::agent::AgentClient>,
    source: crate::session::SessionSource,
    owner_key: &str,
    title: Option<String>,
) -> anyhow::Result<ResetResult> {
    for _ in 0..32 {
        let route_key = deps.store.session_key_for_source(&source);
        let route_lease = deps
            .route_leases
            .acquire(
                &route_key,
                owner_key,
                deps.generation.fetch_add(1, Ordering::Relaxed),
                None,
            )
            .await
            .map_err(|error| anyhow::anyhow!(error))?;
        let observe_store = deps.store.clone();
        let observe_source = source.clone();
        let expected = tokio::task::spawn_blocking(move || {
            observe_store.current_entry_for_source(&observe_source)
        })
        .await
        .map_err(|error| anyhow::anyhow!("reset route observer failed: {error}"))?;

        let lease = if let Some(entry) = expected.as_ref() {
            deps.transcript_leases
                .acquire(
                    &entry.session_id,
                    owner_key,
                    deps.generation.fetch_add(1, Ordering::Relaxed),
                    None,
                )
                .await
                .map_err(|error| anyhow::anyhow!(error))?
        } else {
            None
        };

        let reset_store = deps.store.clone();
        let reset_source = source.clone();
        let reset_expected = expected.clone();
        let reset = tokio::task::spawn_blocking(move || {
            reset_store.reset_session(&reset_source, reset_expected.as_ref())
        })
        .await
        .map_err(|error| anyhow::anyhow!("reset worker failed: {error}"))??;
        let Some(reset) = reset else {
            drop(lease);
            drop(route_lease);
            continue;
        };

        if let Some(predecessor_id) = reset.predecessor_id.as_deref() {
            let database = deps.store.database_for_key(&reset.entry.session_key);
            agent.retire_conversation(
                crate::agent::TurnContext::from_database(database.as_deref()),
                predecessor_id,
            );
        }
        drop(lease);
        drop(route_lease);

        let mut reply = if reset.predecessor_id.is_some() {
            "✨ Session reset! Starting fresh.".to_owned()
        } else {
            "✨ New session started!".to_owned()
        };
        if title.is_some() {
            reply.push_str("\n\nSession titles are not available in the native gateway yet.");
        }
        return Ok(ResetResult { reply });
    }
    anyhow::bail!("session route kept changing during reset")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_arguments_follow_gateway_shell_and_wrapper_rules() {
        let parsed = parse_resume_request("--all \"Project A Plan\"", false).unwrap();
        assert_eq!(parsed.target.as_deref(), Some("Project A Plan"));
        assert!(parsed.allow_all);
        assert!(parsed.search_query.is_none());

        let parsed = parse_resume_request("full all [Project B all]", true).unwrap();
        assert_eq!(parsed.target.as_deref(), Some("Project B all"));
        assert!(parsed.allow_all);
        assert!(parsed.include_unnamed);

        let parsed = parse_resume_request("search cache repair", true).unwrap();
        assert!(parsed.target.is_none());
        assert_eq!(parsed.search_query.as_deref(), Some("cache repair"));
        let parsed = parse_resume_request("search", true).unwrap();
        assert_eq!(parsed.search_query.as_deref(), Some(""));
        let parsed = parse_resume_request("Project C --cross-room", true).unwrap();
        assert_eq!(parsed.target.as_deref(), Some("Project C"));
        assert!(parsed.allow_cross_room);
        assert!(parse_resume_request("\"unterminated", false).is_err());
    }
}
