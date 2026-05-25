use super::{Session, Sub, UnreadCounts, EDIT_TAG_BATCH_SIZE, READING_LIST};
use crate::db;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use reqwest::{Client, StatusCode};
use rusqlite::Connection;
use serde::Deserialize;
use tauri::{AppHandle, Manager};

const SUBSCRIPTION_BATCH_SIZE: usize = 50;

#[derive(Deserialize)]
struct QuickAddResponse {
    #[serde(default, rename = "numResults")]
    num_results: i64,
}

struct FeedPushFailure {
    message: String,
    transient: bool,
}

impl FeedPushFailure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: true,
        }
    }

    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: false,
        }
    }
}

fn transient_status(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::UNAUTHORIZED
        || status == StatusCode::FORBIDDEN
}

fn remote_id_tail(remote_id: &str) -> &str {
    remote_id.rsplit('/').next().unwrap_or(remote_id)
}

pub(super) fn item_decimal_id(remote_id: &str) -> Option<u64> {
    let tail = remote_id_tail(remote_id);
    if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_hexdigit()) {
        u64::from_str_radix(tail, 16).ok()
    } else {
        None
    }
}

pub(super) fn edit_tag_item_id(remote_id: &str) -> String {
    if let Some(id) = item_decimal_id(remote_id) {
        return id.to_string();
    }
    remote_id.to_string()
}

pub(super) fn edit_tag_batch_size(field: &str, value: bool) -> usize {
    if field == "read" && value {
        // FreshRSS returns "OK" even when its bulk markRead path affects no rows.
        // Keep mark-read batches below six so FreshRSS takes its single-entry path.
        5
    } else {
        EDIT_TAG_BATCH_SIZE
    }
}

pub(super) fn mark_all_read_fallback_id(
    remote_unread_after_push: usize,
    unread_stream_articles: usize,
    queued_local_read_pushes: usize,
    imported_articles: usize,
    skipped_unread_articles: usize,
    max_unread_item_id: Option<u64>,
) -> Option<u64> {
    if remote_unread_after_push > 0
        && remote_unread_after_push == unread_stream_articles
        && queued_local_read_pushes == unread_stream_articles
        && imported_articles == 0
        && skipped_unread_articles == 0
    {
        max_unread_item_id
    } else {
        None
    }
}

pub(super) async fn remote_unread_count(
    http: &Client,
    session: &Session,
) -> AppResult<Option<usize>> {
    let counts: UnreadCounts = session
        .get(http, "unread-count?output=json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(reading_list_unread_count(&counts))
}

fn reading_list_unread_count(counts: &UnreadCounts) -> Option<usize> {
    counts
        .unreadcounts
        .iter()
        .find(|entry| entry.id == READING_LIST)
        .map(|entry| entry.count)
        .or(counts.max)
}

fn category_stream_label(id: &str) -> Option<&str> {
    id.strip_prefix("user/-/label/")
        .or_else(|| id.split_once("/label/").map(|(_, label)| label))
}

fn is_default_category(name: &str) -> bool {
    let name = name.trim();
    name.is_empty()
        || name.eq_ignore_ascii_case("uncategorized")
        || name.eq_ignore_ascii_case("uncategorised")
        || matches!(name, "未分类" | "未分類")
}

fn subscription_folder_name(sub: &Sub) -> Option<String> {
    sub.categories.iter().find_map(|category| {
        if category
            .kind
            .as_deref()
            .is_some_and(|kind| !kind.eq_ignore_ascii_case("folder"))
        {
            return None;
        }
        let name = category
            .label
            .as_deref()
            .or_else(|| category.id.as_deref().and_then(category_stream_label))?
            .trim();
        if is_default_category(name) {
            None
        } else {
            Some(name.to_string())
        }
    })
}

pub(super) fn subscription_folder_id(conn: &Connection, sub: &Sub) -> AppResult<Option<i64>> {
    subscription_folder_name(sub)
        .map(|name| db::folder_id_by_name(conn, &name))
        .transpose()
}

pub(super) async fn mark_all_reading_list_as_read(
    http: &Client,
    session: &Session,
    older_than_id: u64,
) -> AppResult<()> {
    let ts = older_than_id.to_string();
    let resp = session
        .post(http, "mark-all-as-read")
        .form(&[
            ("s", READING_LIST),
            ("ts", ts.as_str()),
            ("T", session.token.as_str()),
        ])
        .send()
        .await?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("<body error: {e}>"));
    if status.is_success() && body.trim() == "OK" {
        Ok(())
    } else {
        Err(AppError::other(format!(
            "FreshRSS mark-all-as-read failed: HTTP {status}: {body}"
        )))
    }
}

async fn push_subscription_entry(
    http: &Client,
    session: &Session,
    entry: &db::FreshrssFeedSyncEntry,
) -> Result<(), FeedPushFailure> {
    match entry.action.as_str() {
        db::FRESHRSS_FEED_ACTION_SUBSCRIBE => {
            let resp = session
                .post(http, "subscription/quickadd")
                .form(&[
                    ("quickadd", entry.feed_url.as_str()),
                    ("T", session.token.as_str()),
                ])
                .send()
                .await
                .map_err(|e| FeedPushFailure::transient(e.to_string()))?;
            let status = resp.status();
            let body = resp
                .text()
                .await
                .map_err(|e| FeedPushFailure::transient(e.to_string()))?;
            if !status.is_success() {
                return Err(if transient_status(status) {
                    FeedPushFailure::transient(format!("quickadd HTTP {status}: {body}"))
                } else {
                    FeedPushFailure::permanent(format!("quickadd HTTP {status}: {body}"))
                });
            }
            let parsed: QuickAddResponse = serde_json::from_str(&body).map_err(|e| {
                FeedPushFailure::permanent(format!(
                    "quickadd response parse failed: {e}; body: {body}"
                ))
            })?;
            if parsed.num_results >= 1 {
                Ok(())
            } else {
                Err(FeedPushFailure::permanent(format!(
                    "quickadd returned numResults={}",
                    parsed.num_results
                )))
            }
        }
        db::FRESHRSS_FEED_ACTION_UNSUBSCRIBE => {
            let stream_id = format!("feed/{}", entry.feed_url);
            let resp = session
                .post(http, "subscription/edit")
                .form(&[
                    ("ac", "unsubscribe"),
                    ("s", stream_id.as_str()),
                    ("T", session.token.as_str()),
                ])
                .send()
                .await
                .map_err(|e| FeedPushFailure::transient(e.to_string()))?;
            let status = resp.status();
            let body = resp
                .text()
                .await
                .map_err(|e| FeedPushFailure::transient(e.to_string()))?;
            if status.is_success() {
                if body.trim() == "OK" {
                    Ok(())
                } else {
                    Err(FeedPushFailure::permanent(format!(
                        "unsubscribe HTTP {status} without OK body: {body}"
                    )))
                }
            } else if status.is_client_error() && !transient_status(status) {
                Ok(())
            } else {
                Err(if transient_status(status) {
                    FeedPushFailure::transient(format!("unsubscribe HTTP {status}: {body}"))
                } else {
                    FeedPushFailure::permanent(format!("unsubscribe HTTP {status}: {body}"))
                })
            }
        }
        other => Err(FeedPushFailure::permanent(format!(
            "unknown FreshRSS subscription action: {other}"
        ))),
    }
}

/// Push a bounded batch of local feed add/delete mutations to FreshRSS. Returns
/// whether more non-terminal subscription work remains and another debounced
/// sync should be scheduled.
pub(super) async fn push_subscription_queue(
    app: &AppHandle,
    http: &Client,
    session: &Session,
) -> AppResult<bool> {
    let entries = {
        let state = app.state::<AppState>();
        let conn = state.db.lock().await;
        db::freshrss_feed_sync_entries(&conn, SUBSCRIPTION_BATCH_SIZE)?
    };
    if entries.is_empty() {
        return Ok(false);
    }

    let mut transient_failed = false;
    for entry in entries {
        match push_subscription_entry(http, session, &entry).await {
            Ok(()) => {
                let state = app.state::<AppState>();
                let conn = state.db.lock().await;
                db::clear_freshrss_feed_sync_entry(&conn, &entry.feed_url, &entry.action)?;
            }
            Err(e) => {
                let state = app.state::<AppState>();
                let conn = state.db.lock().await;
                let still_current = db::mark_freshrss_feed_sync_failure(
                    &conn,
                    &entry.feed_url,
                    &entry.action,
                    &e.message,
                )?;
                if still_current {
                    transient_failed |= e.transient;
                    log::warn!(
                        "sync: FreshRSS {} failed for {}: {}",
                        entry.action,
                        entry.feed_url,
                        e.message
                    );
                }
            }
        }
    }

    let state = app.state::<AppState>();
    let conn = state.db.lock().await;
    Ok(!transient_failed && db::has_freshrss_feed_sync_work(&conn)?)
}

#[cfg(test)]
mod tests {
    use super::super::{Sub, SubscriptionCategory, UnreadCount};
    use super::*;

    fn subscription(feed_url: &str, title: &str, folder: Option<&str>) -> Sub {
        Sub {
            id: Some(format!("feed/{feed_url}")),
            url: Some(feed_url.to_string()),
            title: Some(title.to_string()),
            categories: folder
                .map(|name| {
                    vec![SubscriptionCategory {
                        id: Some(format!("user/-/label/{name}")),
                        label: Some(name.to_string()),
                        kind: Some("folder".to_string()),
                    }]
                })
                .unwrap_or_default(),
        }
    }

    #[test]
    fn reading_list_unread_count_prefers_reading_list_entry() {
        let counts = UnreadCounts {
            max: Some(99),
            unreadcounts: vec![UnreadCount {
                id: READING_LIST.to_string(),
                count: 131,
            }],
        };

        assert_eq!(reading_list_unread_count(&counts), Some(131));
    }

    #[test]
    fn subscription_folder_name_uses_folder_categories_only() {
        let sub = Sub {
            id: None,
            url: Some("https://example.com/feed.xml".to_string()),
            title: None,
            categories: vec![
                SubscriptionCategory {
                    id: Some("user/-/label/Read Later".to_string()),
                    label: Some("Read Later".to_string()),
                    kind: Some("tag".to_string()),
                },
                SubscriptionCategory {
                    id: Some("user/-/label/Tech".to_string()),
                    label: None,
                    kind: Some("folder".to_string()),
                },
            ],
        };

        assert_eq!(subscription_folder_name(&sub).as_deref(), Some("Tech"));
    }

    #[test]
    fn subscription_folder_name_ignores_default_category() {
        for name in ["Uncategorized", "uncategorised", "未分类", "未分類"] {
            let sub = subscription("https://example.com/feed.xml", "Example", Some(name));
            assert_eq!(subscription_folder_name(&sub), None);
        }
    }

    #[test]
    fn remote_id_tail_keeps_the_freshrss_item_id_visible() {
        assert_eq!(
            remote_id_tail("tag:google.com,2005:reader/item/000631d2b3c4a500"),
            "000631d2b3c4a500"
        );
        assert_eq!(remote_id_tail("plain-id"), "plain-id");
    }

    #[test]
    fn edit_tag_item_id_sends_decimal_entry_ids() {
        assert_eq!(
            edit_tag_item_id("tag:google.com,2005:reader/item/0006529d48ab0e0c"),
            "1779685342776844"
        );
        assert_eq!(edit_tag_item_id("not-a-hex-item"), "not-a-hex-item");
    }

    #[test]
    fn mark_all_read_fallback_only_when_all_remote_unread_are_local_read() {
        assert_eq!(
            mark_all_read_fallback_id(20, 20, 20, 0, 0, Some(42)),
            Some(42)
        );
        assert_eq!(mark_all_read_fallback_id(19, 20, 20, 0, 0, Some(42)), None);
        assert_eq!(mark_all_read_fallback_id(20, 20, 19, 0, 0, Some(42)), None);
        assert_eq!(mark_all_read_fallback_id(20, 20, 20, 1, 0, Some(42)), None);
        assert_eq!(mark_all_read_fallback_id(20, 20, 20, 0, 1, Some(42)), None);
    }
}
