//! Synchronisation over the Google Reader compatible API.
//!
//! Supports any GReader-compatible backend; today FreshRSS and Miniflux.
//! Protocol is identical (`ClientLogin`, `reader/api/0/edit-tag`,
//! `stream/contents/...`, `com.google/*` tags) — only the API root path
//! differs per provider, so the `Provider` enum centralises that mapping.
//!
//! Flow: `ClientLogin` for an auth token, push any queued local read/starred
//! changes via `edit-tag`, push FreshRSS feed add/delete mutations, then pull
//! the subscription list (to subscribe locally to new feeds) and the recent
//! reading-list (to reconcile read/starred state, matched to local articles by
//! URL). A second article-state push runs after reconciliation so queued changes
//! for articles that just gained a remote id reach the server in the same sync.

mod freshrss;

use crate::db;
use crate::error::{AppError, AppResult};
use crate::ingestion::parse;
use crate::models::Enclosure;
use crate::sanitize;
use crate::state::AppState;
use reqwest::{Client, RequestBuilder};
use rusqlite::Connection;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

const READ_TAG: &str = "user/-/state/com.google/read";
const STARRED_TAG: &str = "user/-/state/com.google/starred";
const READING_LIST: &str = "user/-/state/com.google/reading-list";
const AUTO_SYNC_DEBOUNCE: Duration = Duration::from_secs(5);
const EDIT_TAG_BATCH_SIZE: usize = 100;
const STREAM_BATCH_SIZE: usize = 1000;
const STREAM_PAGE_LIMIT: usize = 50;

/// Debounce guard: `true` while a sleeper task is pending, so concurrent
/// triggers coalesce into the one already scheduled.
static AUTO_SYNC_SCHEDULED: AtomicBool = AtomicBool::new(false);
/// Monotonic counter bumped each time a real sync runs. A spawned sleeper
/// records the value at scheduling time; if the counter has advanced when it
/// wakes, a manual or refresh-driven sync already happened during the sleep
/// and this trigger is redundant. Cancelling the sleep itself would need a
/// `Notify`/abort handle; checking after wake is simpler and has the same
/// effect (one wasted timer, no wasted HTTP).
static SYNC_GENERATION: AtomicU64 = AtomicU64::new(0);

fn edit_tag_item_id(provider: Provider, remote_id: &str) -> String {
    if provider == Provider::FreshRss {
        freshrss::edit_tag_item_id(remote_id)
    } else {
        remote_id.to_string()
    }
}

fn edit_tag_batch_size(provider: Provider, field: &str, value: bool) -> usize {
    if provider == Provider::FreshRss {
        freshrss::edit_tag_batch_size(field, value)
    } else {
        EDIT_TAG_BATCH_SIZE
    }
}

fn reading_list_contents_path(continuation: Option<&str>, unread_only: bool) -> String {
    let mut path = format!("stream/contents/{READING_LIST}?output=json&n={STREAM_BATCH_SIZE}");
    if unread_only {
        path.push_str("&xt=");
        path.push_str(READ_TAG);
    }
    if let Some(c) = continuation.filter(|c| !c.is_empty()) {
        path.push_str("&c=");
        path.extend(url::form_urlencoded::byte_serialize(c.as_bytes()));
    }
    path
}

/// Which GReader-compatible backend the user is connected to. The wire
/// protocol is identical; only where the API root sits under the server URL
/// differs (FreshRSS mounts it at `/api/greader.php`, Miniflux serves it at
/// the server root).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Provider {
    FreshRss,
    Miniflux,
}

impl Provider {
    /// Path segment to append to the user-supplied server URL to reach the
    /// GReader API root. Miniflux serves `/accounts/ClientLogin` and
    /// `/reader/api/0/...` straight off the server root, so its suffix is
    /// empty.
    fn path_suffix(self) -> &'static str {
        match self {
            Provider::FreshRss => "/api/greader.php",
            Provider::Miniflux => "",
        }
    }

    /// Parse the persisted setting. Missing / unknown → FreshRss, so older
    /// installs (where this setting didn't exist) keep working unchanged.
    fn from_setting(s: Option<&str>) -> Self {
        match s.unwrap_or("").trim() {
            "miniflux" => Provider::Miniflux,
            _ => Provider::FreshRss,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Provider::FreshRss => "freshrss",
            Provider::Miniflux => "miniflux",
        }
    }
}

fn should_try_mark_all_read_fallback(
    provider: Provider,
    unread_item_id_parse_failures: usize,
    unread_stream_hit_page_limit: bool,
) -> bool {
    provider == Provider::FreshRss
        && unread_item_id_parse_failures == 0
        && !unread_stream_hit_page_limit
}

/// Normalise a user-supplied server URL to its GReader API root for the
/// chosen provider. Idempotent: if the user already typed the full path,
/// don't append it again.
fn greader_base(url: &str, provider: Provider) -> String {
    let t = url.trim().trim_end_matches('/');
    let suffix = provider.path_suffix();
    if t.ends_with(suffix) || t.contains(&format!("{suffix}/")) {
        t.to_string()
    } else {
        format!("{t}{suffix}")
    }
}

/// An authenticated FreshRSS session.
struct Session {
    base: String,
    auth: String,
    token: String,
}

impl Session {
    fn get(&self, http: &Client, path: &str) -> RequestBuilder {
        http.get(format!("{}/reader/api/0/{path}", self.base))
            .header("Authorization", format!("GoogleLogin auth={}", self.auth))
    }
    fn post(&self, http: &Client, path: &str) -> RequestBuilder {
        http.post(format!("{}/reader/api/0/{path}", self.base))
            .header("Authorization", format!("GoogleLogin auth={}", self.auth))
    }
}

/// Exchange username + password for a long-lived auth token.
async fn client_login(http: &Client, base: &str, user: &str, pass: &str) -> AppResult<String> {
    let resp = http
        .post(format!("{base}/accounts/ClientLogin"))
        .form(&[("Email", user), ("Passwd", pass)])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(AppError::code("freshrssLoginFailed"));
    }
    let body = resp.text().await?;
    body.lines()
        .find_map(|l| l.strip_prefix("Auth="))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::code("freshrssNoToken"))
}

/// Build a session from an existing auth token by fetching a fresh write
/// (edit-tag) token. Fails fast if the auth token is no longer valid.
async fn session_with_token(http: &Client, base: &str, auth: String) -> AppResult<Session> {
    let token = http
        .get(format!("{base}/reader/api/0/token"))
        .header("Authorization", format!("GoogleLogin auth={auth}"))
        .send()
        .await?
        .error_for_status()
        .map_err(|_| AppError::code("freshrssLoginFailed"))?
        .text()
        .await?
        .trim()
        .to_string();
    Ok(Session {
        base: base.to_string(),
        auth,
        token,
    })
}

/// Log in with username + password and obtain a full session.
async fn login(http: &Client, base: &str, user: &str, pass: &str) -> AppResult<Session> {
    let auth = client_login(http, base, user, pass).await?;
    session_with_token(http, base, auth).await
}

#[derive(Deserialize)]
struct SubList {
    #[serde(default)]
    subscriptions: Vec<Sub>,
}
#[derive(Deserialize)]
struct Sub {
    id: Option<String>,
    url: Option<String>,
    title: Option<String>,
    #[serde(default)]
    categories: Vec<SubscriptionCategory>,
}

#[derive(Deserialize)]
struct SubscriptionCategory {
    id: Option<String>,
    label: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct UnreadCounts {
    max: Option<usize>,
    #[serde(default)]
    unreadcounts: Vec<UnreadCount>,
}
#[derive(Deserialize)]
struct UnreadCount {
    id: String,
    count: usize,
}

#[derive(Deserialize)]
struct Contents {
    #[serde(default)]
    items: Vec<Item>,
    continuation: Option<String>,
}
#[derive(Deserialize)]
struct Item {
    id: String,
    title: Option<String>,
    author: Option<String>,
    published: Option<i64>,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    canonical: Vec<Href>,
    #[serde(default)]
    alternate: Vec<Href>,
    summary: Option<TextContent>,
    content: Option<TextContent>,
    origin: Option<Origin>,
    #[serde(default)]
    enclosure: Vec<RemoteEnclosure>,
}
#[derive(Deserialize)]
struct Href {
    href: String,
}
#[derive(Deserialize)]
struct TextContent {
    content: Option<String>,
}
#[derive(Deserialize)]
struct Origin {
    #[serde(rename = "streamId")]
    stream_id: Option<String>,
    #[serde(rename = "htmlUrl")]
    html_url: Option<String>,
}
#[derive(Deserialize)]
struct RemoteEnclosure {
    href: Option<String>,
    url: Option<String>,
    #[serde(rename = "type")]
    mime_type: Option<String>,
    length: Option<i64>,
}

#[derive(Default)]
pub struct SyncStats {
    pub reconciled_articles: usize,
    pub imported_articles: usize,
    pub remote_unread_articles: usize,
    pub matched_unread_articles: usize,
    pub skipped_unread_articles: usize,
    pub skipped_unread_no_stream: usize,
    pub skipped_unread_no_feed: usize,
    pub skipped_unread_no_identity: usize,
}

/// Stored GReader connection. We persist the long-lived auth token rather
/// than the password — a leaked token is revocable server-side and can't be
/// replayed against the user's other accounts. `legacy_pass` holds a
/// plaintext password from an older install, awaiting one-time migration.
struct Creds {
    url: String,
    user: String,
    auth: Option<String>,
    legacy_pass: Option<String>,
    provider: Provider,
}

/// Stored GReader credentials, if a server is configured. The setting keys
/// are still named `freshrss_*` for backwards compatibility with installs
/// that predate multi-provider support — the values are provider-agnostic.
async fn creds(app: &AppHandle) -> AppResult<Option<Creds>> {
    let state = app.state::<AppState>();
    let conn = state.db.lock().await;
    let url = db::get_setting(&conn, "freshrss_url")?.unwrap_or_default();
    let user = db::get_setting(&conn, "freshrss_user")?.unwrap_or_default();
    let nonempty = |k| db::get_setting(&conn, k).map(|v| v.filter(|s| !s.is_empty()));
    let auth = nonempty("freshrss_auth")?;
    let legacy_pass = nonempty("freshrss_pass")?;
    let provider = Provider::from_setting(db::get_setting(&conn, "freshrss_provider")?.as_deref());
    if url.trim().is_empty() || user.is_empty() || (auth.is_none() && legacy_pass.is_none()) {
        return Ok(None);
    }
    Ok(Some(Creds {
        url,
        user,
        auth,
        legacy_pass,
        provider,
    }))
}

/// The configured GReader server URL and provider, or `None` when not
/// connected.
pub async fn connected_url(app: &AppHandle) -> AppResult<Option<(String, String)>> {
    Ok(creds(app)
        .await?
        .map(|c| (c.url, c.provider.as_str().to_string())))
}

/// Persist a verified connection, storing the auth token and never the
/// password (any legacy stored password is also cleared).
async fn persist_session(
    app: &AppHandle,
    url: &str,
    user: &str,
    auth: &str,
    provider: Provider,
) -> AppResult<()> {
    let state = app.state::<AppState>();
    let conn = state.db.lock().await;
    db::set_setting(&conn, "freshrss_url", url.trim())?;
    db::set_setting(&conn, "freshrss_user", user)?;
    db::set_setting(&conn, "freshrss_auth", auth)?;
    db::set_setting(&conn, "freshrss_pass", "")?;
    db::set_setting(&conn, "freshrss_provider", provider.as_str())?;
    Ok(())
}

/// Verify credentials against the server and, on success, persist them.
pub async fn connect(
    app: &AppHandle,
    url: &str,
    user: &str,
    pass: &str,
    provider: Option<&str>,
) -> AppResult<()> {
    let provider = Provider::from_setting(provider);
    let base = greader_base(url, provider);
    let http = app.state::<AppState>().http();
    let session = login(&http, &base, user, pass).await?; // verifies credentials
    persist_session(app, url, user, &session.auth, provider).await
}

/// Forget the stored GReader credentials.
pub async fn disconnect(app: &AppHandle) -> AppResult<()> {
    let state = app.state::<AppState>();
    let conn = state.db.lock().await;
    for key in [
        "freshrss_url",
        "freshrss_user",
        "freshrss_auth",
        "freshrss_pass",
        "freshrss_provider",
    ] {
        db::set_setting(&conn, key, "")?;
    }
    db::clear_freshrss_feed_sync_queue(&conn)?;
    Ok(())
}

/// Run a full sync if a server is connected. Returns `true` when a sync
/// actually ran, so the caller can refresh the UI for the reconciled state.
pub async fn run_if_connected(app: &AppHandle) -> AppResult<bool> {
    if creds(app).await?.is_some() {
        sync_now(app).await.map(|_| true)
    } else {
        Ok(false)
    }
}

/// Schedule a near-real-time sync after a local queued edit. The durable DB
/// queue remains the source of truth; this only decides when to flush it.
pub fn trigger_soon(app: AppHandle) {
    if AUTO_SYNC_SCHEDULED.swap(true, Ordering::AcqRel) {
        return;
    }
    let scheduled_at = SYNC_GENERATION.load(Ordering::Acquire);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(AUTO_SYNC_DEBOUNCE).await;
        AUTO_SYNC_SCHEDULED.store(false, Ordering::Release);
        // Pre-lock generation check: a sync already drained the queue between
        // scheduling and now, so this trigger has nothing to do. The real
        // guarantee comes from the post-lock check inside `sync_now_inner`,
        // which catches a manual sync that's mid-login when we wake.
        if SYNC_GENERATION.load(Ordering::Acquire) != scheduled_at {
            return;
        }
        match creds(&app).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return;
            }
            Err(e) => {
                log::warn!("queued sync skipped — creds lookup failed: {e}");
                return;
            }
        }
        match sync_now_inner(&app, Some(scheduled_at)).await {
            Ok(Some(_)) => {
                let _ = app.emit("feeds-updated", 0);
                crate::notify::update_badge(&app).await;
                crate::tray::refresh(&app).await;
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!("queued sync failed: {e}");
            }
        }
    });
}

/// Flush queued local read/starred changes whose articles already have a
/// remote GReader id. Rows without a remote id are intentionally left queued;
/// the pull step may assign those ids later in the same sync.
async fn push_sync_queue(
    app: &AppHandle,
    http: &Client,
    session: &Session,
    provider: Provider,
) -> AppResult<()> {
    let mut queue = {
        let state = app.state::<AppState>();
        let conn = state.db.lock().await;
        let q = db::take_sync_queue(&conn)?;
        // Bump under the DB lock, atomically with the drain. If we bumped
        // after releasing the lock, an edit could enqueue in the gap, then
        // its `trigger_soon` would read the pre-bump generation while this
        // drain went on to bump — a sleeper that then wakes would compare
        // its (pre-bump) scheduled_at to the (post-bump) generation, mismatch,
        // and skip its run even though its edit was never drained.
        SYNC_GENERATION.fetch_add(1, Ordering::AcqRel);
        q
    };
    if queue.is_empty() {
        return Ok(());
    }

    queue.sort_by(|a, b| (a.field.as_str(), a.value).cmp(&(b.field.as_str(), b.value)));

    let mut failed: Vec<&db::SyncEntry> = Vec::new();
    let mut cursor = 0usize;
    while cursor < queue.len() {
        let field = queue[cursor].field.as_str();
        let value = queue[cursor].value;
        let mut end = cursor + 1;
        while end < queue.len() && queue[end].field == field && queue[end].value == value {
            end += 1;
        }

        let tag = if field == "starred" {
            STARRED_TAG
        } else {
            READ_TAG
        };
        let action = if value { "a" } else { "r" };
        let batch_size = edit_tag_batch_size(provider, field, value);
        for chunk in queue[cursor..end].chunks(batch_size) {
            let mut form = Vec::with_capacity(chunk.len() + 2);
            form.push((action.to_string(), tag.to_string()));
            form.push(("T".to_string(), session.token.clone()));
            for entry in chunk {
                let item_id = edit_tag_item_id(provider, entry.remote_id.as_str());
                form.push(("i".to_string(), item_id));
            }

            match session.post(http, "edit-tag").form(&form).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        log::warn!(
                            "sync: edit-tag push failed ({field}={value}, {} ids): HTTP {status}",
                            chunk.len()
                        );
                        failed.extend(chunk.iter());
                    }
                }
                Err(e) => {
                    log::warn!(
                        "sync: edit-tag push failed ({field}={value}, {} ids): {e}",
                        chunk.len()
                    );
                    failed.extend(chunk.iter());
                }
            }
        }

        cursor = end;
    }

    if !failed.is_empty() {
        log::warn!("sync: {} change(s) failed to push, re-queued", failed.len());
        let state = app.state::<AppState>();
        let conn = state.db.lock().await;
        for entry in failed {
            if let Err(e) = db::requeue_sync(&conn, entry.article_id, &entry.field, entry.value) {
                log::error!(
                    "sync: requeue failed for article {} ({}={}): {e} — change lost",
                    entry.article_id,
                    entry.field,
                    entry.value
                );
            }
        }
    }
    Ok(())
}

fn item_url(item: &Item) -> Option<String> {
    item.canonical
        .first()
        .or_else(|| item.alternate.first())
        .map(|h| h.href.trim().to_string())
        .filter(|u| !u.is_empty())
}

fn item_is_read(item: &Item) -> bool {
    item.categories.iter().any(|c| c == READ_TAG)
}

fn item_is_starred(item: &Item) -> bool {
    item.categories.iter().any(|c| c == STARRED_TAG)
}

fn item_remote_stream_id(item: &Item) -> Option<&str> {
    item.origin
        .as_ref()
        .and_then(|origin| origin.stream_id.as_deref())
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

fn epoch_to_rfc3339(seconds: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0)
        .map(parse::clamp_publish_date)
        .map(|d| d.to_rfc3339())
}

fn remote_item_article(item: &Item) -> Option<db::NewArticle> {
    let url = item_url(item);
    let raw_html = item
        .content
        .as_ref()
        .and_then(|c| c.content.as_ref())
        .or_else(|| item.summary.as_ref().and_then(|s| s.content.as_ref()))
        .cloned()
        .unwrap_or_default();
    let base = url.as_deref().or_else(|| {
        item.origin
            .as_ref()
            .and_then(|origin| origin.html_url.as_deref())
    });
    let content_html = if raw_html.trim().is_empty() {
        None
    } else {
        Some(sanitize::sanitize(&raw_html, base))
    };
    let body_text = sanitize::html_to_text(&raw_html);
    let title = item
        .title
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| url.clone())
        .unwrap_or_else(|| "(untitled)".to_string());
    let guid = if item.id.trim().is_empty() {
        url.clone()?
    } else {
        item.id.clone()
    };
    let image_url = content_html.as_deref().and_then(sanitize::first_image);
    let enclosures = item
        .enclosure
        .iter()
        .filter_map(|e| {
            let url = e
                .href
                .as_deref()
                .or(e.url.as_deref())
                .map(str::trim)
                .filter(|u| !u.is_empty())?
                .to_string();
            Some(Enclosure {
                url,
                mime_type: e.mime_type.clone(),
                length: e.length,
            })
        })
        .collect();

    Some(db::NewArticle {
        guid,
        url,
        title,
        author: item.author.clone().filter(|s| !s.trim().is_empty()),
        summary: item
            .summary
            .as_ref()
            .and_then(|s| s.content.as_ref())
            .map(|s| sanitize::html_to_text(s))
            .filter(|s| !s.is_empty()),
        content_html,
        body_text,
        image_url,
        published_at: item.published.and_then(epoch_to_rfc3339),
        enclosures,
    })
}

#[derive(Default)]
struct RemoteItemApply {
    matched: bool,
    imported: bool,
    state_changed: bool,
    pending_gained_remote_id: bool,
    queued_local_read_push: bool,
    skipped_no_stream: bool,
    skipped_no_feed: bool,
    skipped_no_identity: bool,
}

fn apply_remote_item(
    conn: &Connection,
    item: &Item,
    remote_feed_ids: &HashMap<String, i64>,
    pending: &HashSet<i64>,
    import_missing_unread: bool,
) -> AppResult<RemoteItemApply> {
    let read = item_is_read(item);
    let starred = item_is_starred(item);
    if let Some(url) = item_url(item) {
        if let Some(aid) = db::article_id_by_url(conn, &url)? {
            db::set_remote_id(conn, aid, &item.id)?;
            let pending_gained_remote_id = pending.contains(&aid);
            if pending_gained_remote_id {
                return Ok(RemoteItemApply {
                    matched: true,
                    pending_gained_remote_id,
                    ..RemoteItemApply::default()
                });
            }
            if import_missing_unread && !read {
                let (local_read, _local_starred) = db::article_sync_state(conn, aid)?;
                if local_read {
                    db::enqueue_sync(conn, aid, "read", true)?;
                    return Ok(RemoteItemApply {
                        matched: true,
                        pending_gained_remote_id: true,
                        queued_local_read_push: true,
                        ..RemoteItemApply::default()
                    });
                }
            }
            let state_changed = db::set_sync_state(conn, aid, read, starred)?;
            return Ok(RemoteItemApply {
                matched: true,
                state_changed,
                ..RemoteItemApply::default()
            });
        }
    }

    if !import_missing_unread || read {
        return Ok(RemoteItemApply::default());
    }
    let Some(stream_id) = item_remote_stream_id(item) else {
        return Ok(RemoteItemApply {
            skipped_no_stream: true,
            ..RemoteItemApply::default()
        });
    };
    let Some(feed_id) = remote_feed_ids.get(stream_id).copied() else {
        return Ok(RemoteItemApply {
            skipped_no_feed: true,
            ..RemoteItemApply::default()
        });
    };
    let Some(article) = remote_item_article(item) else {
        return Ok(RemoteItemApply {
            skipped_no_identity: true,
            ..RemoteItemApply::default()
        });
    };
    db::upsert_article(conn, feed_id, &article, false, &[])?;
    let aid = match article.url.as_deref() {
        Some(url) => db::article_id_by_url(conn, url)?,
        None => db::article_id_by_feed_guid(conn, feed_id, &article.guid)?,
    };
    if let Some(aid) = aid {
        db::set_remote_id(conn, aid, &item.id)?;
        db::set_sync_state(conn, aid, false, starred)?;
        Ok(RemoteItemApply {
            imported: true,
            ..RemoteItemApply::default()
        })
    } else {
        Ok(RemoteItemApply::default())
    }
}

fn sync_subscription_to_local(
    conn: &Connection,
    provider: Provider,
    sub: &Sub,
    remote_feed_ids: &mut HashMap<String, i64>,
) -> AppResult<()> {
    let Some(feed_url) = sub.url.as_deref().filter(|u| !u.is_empty()) else {
        return Ok(());
    };
    let folder_id = if provider == Provider::FreshRss {
        freshrss::subscription_folder_id(conn, sub)?
    } else {
        None
    };
    let feed_id = if let Some(feed_id) = db::find_feed_by_url(conn, feed_url)? {
        if provider == Provider::FreshRss {
            db::move_feed(conn, feed_id, folder_id)?;
        }
        feed_id
    } else {
        let title = sub.title.as_deref().unwrap_or(feed_url);
        let st = parse::detect_source_type(feed_url);
        db::insert_feed(conn, feed_url, None, title, None, st, folder_id)?
    };
    if let Some(remote_id) = sub.id.as_deref().filter(|s| !s.is_empty()) {
        remote_feed_ids.insert(remote_id.to_string(), feed_id);
    }
    Ok(())
}

/// Push queued changes, then pull subscriptions and read/starred state.
pub async fn sync_now(app: &AppHandle) -> AppResult<SyncStats> {
    Ok(sync_now_inner(app, None).await?.unwrap_or_default())
}

/// Inner body of `sync_now`. When `expected_gen` is `Some`, the run is
/// abandoned after acquiring `sync_lock` if another sync has already advanced
/// the generation — i.e. the debounced caller's edit was drained by whoever
/// got the lock first. `Ok(None)` signals "skipped"; `Ok(Some(stats))` is the
/// normal sync result.
async fn sync_now_inner(
    app: &AppHandle,
    expected_gen: Option<u64>,
) -> AppResult<Option<SyncStats>> {
    let state = app.state::<AppState>();
    let _sync_guard = state.sync_lock.lock().await;
    if let Some(g) = expected_gen {
        if SYNC_GENERATION.load(Ordering::Acquire) != g {
            return Ok(None);
        }
    }
    // A manual sync supersedes any debounced auto-sync that's still mid-sleep.
    // Clearing the flag lets the next local edit schedule a fresh window; the
    // generation bump in `push_sync_queue` tells any already-spawned sleeper
    // to skip its run when it wakes (see `trigger_soon`).
    AUTO_SYNC_SCHEDULED.store(false, Ordering::Release);
    let creds = creds(app)
        .await?
        .ok_or_else(|| AppError::code("freshrssNotConnected"))?;
    let base = greader_base(&creds.url, creds.provider);
    let http = app.state::<AppState>().http();
    let session = match &creds.auth {
        Some(auth) => session_with_token(&http, &base, auth.clone()).await?,
        None => {
            // Legacy install: exchange the plaintext password for a token,
            // then migrate so the password is no longer kept on disk.
            let pass = creds.legacy_pass.as_deref().unwrap_or_default();
            let session = login(&http, &base, &creds.user, pass).await?;
            persist_session(app, &creds.url, &creds.user, &session.auth, creds.provider).await?;
            session
        }
    };

    // 1 ── push: flush queued local read/starred changes. `take_sync_queue`
    // removes pushable rows up front, so any push that fails must be re-queued
    // — otherwise a network blip silently drops the user's change forever.
    push_sync_queue(app, &http, &session, creds.provider).await?;

    // 2 ── push FreshRSS subscription mutations before pulling subscriptions,
    // so a failed local delete is not immediately undone by the pull path below.
    let schedule_more_subscription_sync = if creds.provider == Provider::FreshRss {
        freshrss::push_subscription_queue(app, &http, &session).await?
    } else {
        false
    };

    // 3 ── pull subscriptions: subscribe locally to any feed we don't have,
    // while retaining FreshRSS's feed/<id> stream ids for remote article imports.
    let subs: SubList = session
        .get(&http, "subscription/list?output=json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut remote_feed_ids: HashMap<String, i64> = HashMap::new();
    {
        let state = app.state::<AppState>();
        let conn = state.db.lock().await;
        let pending_unsubscribes: HashSet<String> = db::pending_freshrss_unsubscribe_urls(&conn)?
            .into_iter()
            .collect();
        for sub in subs.subscriptions {
            let Some(feed_url) = sub.url.as_deref().filter(|u| !u.is_empty()) else {
                continue;
            };
            if creds.provider == Provider::FreshRss && pending_unsubscribes.contains(feed_url) {
                continue;
            }
            sync_subscription_to_local(&conn, creds.provider, &sub, &mut remote_feed_ids)?;
        }
    }

    // 4 ── pull remote unread items first. FreshRSS retains unread articles even
    // after the public feed XML has dropped them, so local feed fetching alone
    // cannot make Papr's unread count match FreshRSS.
    let mut imported_articles = 0usize;
    let mut matched_unread_articles = 0usize;
    let mut skipped_unread_no_stream = 0usize;
    let mut skipped_unread_no_feed = 0usize;
    let mut skipped_unread_no_identity = 0usize;
    let mut queued_local_read_pushes = 0usize;
    let mut unread_stream_articles = 0usize;
    let mut max_unread_item_id: Option<u64> = None;
    let mut unread_item_id_parse_failures = 0usize;
    let mut remote_unread_articles = if creds.provider == Provider::FreshRss {
        freshrss::remote_unread_count(&http, &session)
            .await?
            .unwrap_or_default()
    } else {
        0
    };
    // True once a pull phase either makes an existing queued edit pushable by
    // assigning a remote id, or queues a local-read correction for a remote item.
    let mut pending_gained_remote_id = false;
    let mut continuation: Option<String> = None;
    let mut unread_pages = 0usize;
    let mut unread_stream_hit_page_limit = false;
    loop {
        unread_pages += 1;
        let path = reading_list_contents_path(continuation.as_deref(), true);
        let contents: Contents = session
            .get(&http, &path)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let next = contents.continuation.clone().filter(|c| !c.is_empty());
        {
            let state = app.state::<AppState>();
            let conn = state.db.lock().await;
            let pending: HashSet<i64> = db::pending_sync_article_ids(&conn)?.into_iter().collect();
            for item in &contents.items {
                let unread = !item_is_read(item);
                if unread {
                    unread_stream_articles += 1;
                    if creds.provider == Provider::FreshRss {
                        if let Some(item_id) = freshrss::item_decimal_id(&item.id) {
                            max_unread_item_id = Some(
                                max_unread_item_id.map_or(item_id, |current| current.max(item_id)),
                            );
                        } else {
                            unread_item_id_parse_failures += 1;
                        }
                    }
                }
                let applied = apply_remote_item(&conn, item, &remote_feed_ids, &pending, true)?;
                if applied.imported {
                    imported_articles += 1;
                } else if unread && applied.matched {
                    matched_unread_articles += 1;
                } else if applied.skipped_no_stream {
                    skipped_unread_no_stream += 1;
                } else if applied.skipped_no_feed {
                    skipped_unread_no_feed += 1;
                } else if applied.skipped_no_identity {
                    skipped_unread_no_identity += 1;
                }
                if applied.queued_local_read_push {
                    queued_local_read_pushes += 1;
                }
                pending_gained_remote_id |= applied.pending_gained_remote_id;
            }
        }
        match next {
            Some(c) => {
                if unread_pages >= STREAM_PAGE_LIMIT {
                    unread_stream_hit_page_limit = true;
                    log::warn!(
                        "sync: stopping unread stream pagination after {STREAM_PAGE_LIMIT} pages; server kept returning continuation"
                    );
                    break;
                }
                continuation = Some(c);
            }
            None => break,
        }
    }
    if remote_unread_articles == 0 {
        remote_unread_articles = unread_stream_articles;
    }

    // 5 ── pull read/starred state for recent items, matched by URL. This is a
    // bounded recent-window reconciliation; the unread pull above paginates
    // because FreshRSS unread convergence depends on the full unread stream.
    let contents: Contents = session
        .get(&http, &reading_list_contents_path(None, false))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let mut reconciled = 0usize;
    {
        let state = app.state::<AppState>();
        let conn = state.db.lock().await;
        // Articles with still-unsent local edits keep their local state; we
        // only assign their remote id so the next sync can push them.
        let pending: HashSet<i64> = db::pending_sync_article_ids(&conn)?.into_iter().collect();
        for item in contents.items {
            let applied = apply_remote_item(&conn, &item, &remote_feed_ids, &pending, false)?;
            if applied.state_changed {
                reconciled += 1;
            }
            pending_gained_remote_id |= applied.pending_gained_remote_id;
        }
    }

    // 6 ── push again only when the pull made a local change newly pushable.
    // Without this guard the second push runs every sync, taking the DB lock and
    // querying the queue for nothing.
    if pending_gained_remote_id {
        push_sync_queue(app, &http, &session, creds.provider).await?;
        if creds.provider == Provider::FreshRss {
            match freshrss::remote_unread_count(&http, &session).await {
                Ok(Some(count)) => remote_unread_articles = count,
                Ok(None) => {}
                Err(e) => {
                    log::warn!("FreshRSS unread count refresh after second push failed: {e}");
                }
            }
        }
    }
    let skipped_unread_articles =
        skipped_unread_no_stream + skipped_unread_no_feed + skipped_unread_no_identity;
    if should_try_mark_all_read_fallback(
        creds.provider,
        unread_item_id_parse_failures,
        unread_stream_hit_page_limit,
    ) {
        if let Some(older_than_id) = freshrss::mark_all_read_fallback_id(
            remote_unread_articles,
            unread_stream_articles,
            queued_local_read_pushes,
            imported_articles,
            skipped_unread_articles,
            max_unread_item_id,
        ) {
            match freshrss::mark_all_reading_list_as_read(&http, &session, older_than_id).await {
                Ok(()) => match freshrss::remote_unread_count(&http, &session).await {
                    Ok(Some(count)) => {
                        remote_unread_articles = count;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::warn!(
                            "FreshRSS unread count refresh after mark-all fallback failed: {e}"
                        );
                    }
                },
                Err(e) => {
                    log::warn!("FreshRSS mark-all-as-read fallback failed: {e}");
                }
            }
        }
    }
    if schedule_more_subscription_sync {
        trigger_soon(app.clone());
    }
    let stats = SyncStats {
        reconciled_articles: reconciled,
        imported_articles,
        remote_unread_articles,
        matched_unread_articles,
        skipped_unread_articles,
        skipped_unread_no_stream,
        skipped_unread_no_feed,
        skipped_unread_no_identity,
    };
    Ok(Some(stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SourceType;
    use std::path::PathBuf;

    struct TempDb(PathBuf);

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(self.0.with_extension("db-wal"));
            let _ = std::fs::remove_file(self.0.with_extension("db-shm"));
        }
    }

    fn test_conn() -> (Connection, TempDb) {
        let path = std::env::temp_dir().join(format!(
            "papr-sync-test-{}-{}.db",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let conn = db::open(&path).unwrap();
        (conn, TempDb(path))
    }

    fn unread_item(stream_id: &str, url: &str) -> Item {
        Item {
            id: "tag:google.com,2005:reader/item/0000000000000001".to_string(),
            title: Some("Remote unread".to_string()),
            author: Some("FreshRSS".to_string()),
            published: Some(1_700_000_000),
            categories: vec!["user/-/state/com.google/reading-list".to_string()],
            canonical: vec![Href {
                href: url.to_string(),
            }],
            alternate: Vec::new(),
            summary: Some(TextContent {
                content: Some("<p>Hello <strong>world</strong></p>".to_string()),
            }),
            content: None,
            origin: Some(Origin {
                stream_id: Some(stream_id.to_string()),
                html_url: Some("https://example.com".to_string()),
            }),
            enclosure: Vec::new(),
        }
    }

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

    fn feed_folder_name(conn: &Connection, feed_url: &str) -> Option<String> {
        conn.query_row(
            "SELECT fo.name
             FROM feeds f
             LEFT JOIN folders fo ON fo.id = f.folder_id
             WHERE f.feed_url = ?1",
            [feed_url],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn freshrss_subscription_pull_places_new_feed_in_remote_folder() {
        let (conn, _tmp) = test_conn();
        let sub = subscription("https://remote.example/feed.xml", "Remote", Some("Tech"));
        let mut remote_feed_ids = HashMap::new();

        sync_subscription_to_local(&conn, Provider::FreshRss, &sub, &mut remote_feed_ids).unwrap();

        let feed_id = db::find_feed_by_url(&conn, "https://remote.example/feed.xml")
            .unwrap()
            .unwrap();
        assert_eq!(
            feed_folder_name(&conn, "https://remote.example/feed.xml").as_deref(),
            Some("Tech")
        );
        assert_eq!(
            remote_feed_ids.get("feed/https://remote.example/feed.xml"),
            Some(&feed_id)
        );
    }

    #[test]
    fn freshrss_subscription_pull_remote_folder_overwrites_existing_local_move() {
        let (conn, _tmp) = test_conn();
        let local_folder = db::create_folder(&conn, "Local").unwrap();
        db::insert_feed(
            &conn,
            "https://remote.example/feed.xml",
            None,
            "Remote",
            None,
            SourceType::Rss,
            Some(local_folder),
        )
        .unwrap();
        let sub = subscription(
            "https://remote.example/feed.xml",
            "Remote",
            Some("FreshRSS"),
        );
        let mut remote_feed_ids = HashMap::new();

        sync_subscription_to_local(&conn, Provider::FreshRss, &sub, &mut remote_feed_ids).unwrap();

        assert_eq!(
            feed_folder_name(&conn, "https://remote.example/feed.xml").as_deref(),
            Some("FreshRSS")
        );
    }

    #[test]
    fn freshrss_subscription_pull_default_category_clears_local_folder() {
        let (conn, _tmp) = test_conn();
        let local_folder = db::create_folder(&conn, "Local").unwrap();
        db::insert_feed(
            &conn,
            "https://remote.example/feed.xml",
            None,
            "Remote",
            None,
            SourceType::Rss,
            Some(local_folder),
        )
        .unwrap();
        let sub = subscription(
            "https://remote.example/feed.xml",
            "Remote",
            Some("Uncategorized"),
        );
        let mut remote_feed_ids = HashMap::new();

        sync_subscription_to_local(&conn, Provider::FreshRss, &sub, &mut remote_feed_ids).unwrap();

        assert_eq!(
            feed_folder_name(&conn, "https://remote.example/feed.xml"),
            None
        );
    }

    #[test]
    fn freshrss_mark_read_uses_single_entry_compatible_batches() {
        assert_eq!(edit_tag_batch_size(Provider::FreshRss, "read", true), 5);
        assert_eq!(
            edit_tag_batch_size(Provider::FreshRss, "read", false),
            EDIT_TAG_BATCH_SIZE
        );
        assert_eq!(
            edit_tag_batch_size(Provider::FreshRss, "starred", true),
            EDIT_TAG_BATCH_SIZE
        );
        assert_eq!(
            edit_tag_batch_size(Provider::Miniflux, "read", true),
            EDIT_TAG_BATCH_SIZE
        );
    }

    #[test]
    fn freshrss_edit_tag_item_id_sends_decimal_entry_ids() {
        assert_eq!(
            edit_tag_item_id(
                Provider::FreshRss,
                "tag:google.com,2005:reader/item/0006529d48ab0e0c"
            ),
            "1779685342776844"
        );
        assert_eq!(
            edit_tag_item_id(
                Provider::Miniflux,
                "tag:google.com,2005:reader/item/0006529d48ab0e0c"
            ),
            "tag:google.com,2005:reader/item/0006529d48ab0e0c"
        );
        assert_eq!(
            edit_tag_item_id(Provider::FreshRss, "not-a-hex-item"),
            "not-a-hex-item"
        );
    }

    #[test]
    fn reading_list_contents_path_encodes_continuation() {
        let path = reading_list_contents_path(Some("abc&x=1/%+="), true);

        assert_eq!(
            path,
            format!(
                "stream/contents/{READING_LIST}?output=json&n={STREAM_BATCH_SIZE}&xt={READ_TAG}&c=abc%26x%3D1%2F%25%2B%3D"
            )
        );
    }

    #[test]
    fn mark_all_fallback_is_disabled_when_unread_stream_hits_page_limit() {
        assert!(should_try_mark_all_read_fallback(
            Provider::FreshRss,
            0,
            false
        ));
        assert!(!should_try_mark_all_read_fallback(
            Provider::FreshRss,
            0,
            true
        ));
        assert!(!should_try_mark_all_read_fallback(
            Provider::FreshRss,
            1,
            false
        ));
        assert!(!should_try_mark_all_read_fallback(
            Provider::Miniflux,
            0,
            false
        ));
    }

    #[test]
    fn remote_unread_stream_imports_missing_local_article() {
        let (conn, _tmp) = test_conn();
        let feed_id = db::insert_feed(
            &conn,
            "https://example.com/feed.xml",
            Some("https://example.com"),
            "Example",
            None,
            SourceType::Rss,
            None,
        )
        .unwrap();
        let mut remote_feed_ids = HashMap::new();
        remote_feed_ids.insert("feed/42".to_string(), feed_id);

        let item = unread_item("feed/42", "https://example.com/old-unread");
        let applied =
            apply_remote_item(&conn, &item, &remote_feed_ids, &HashSet::new(), true).unwrap();

        assert!(applied.imported);
        assert_eq!(db::count_unread(&conn).unwrap(), 1);
        let (title, is_read, remote_id): (String, bool, String) = conn
            .query_row(
                "SELECT title, is_read, remote_id FROM articles WHERE url = ?1",
                ["https://example.com/old-unread"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(title, "Remote unread");
        assert!(!is_read);
        assert_eq!(remote_id, item.id);
    }

    #[test]
    fn remote_unread_stream_existing_local_read_queues_remote_read_push() {
        let (conn, _tmp) = test_conn();
        let feed_id = db::insert_feed(
            &conn,
            "https://example.com/feed.xml",
            Some("https://example.com"),
            "Example",
            None,
            SourceType::Rss,
            None,
        )
        .unwrap();
        let article = db::NewArticle {
            guid: "local-guid".to_string(),
            url: Some("https://example.com/old-unread".to_string()),
            title: "Local article".to_string(),
            author: None,
            summary: None,
            content_html: None,
            body_text: String::new(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        db::upsert_article(&conn, feed_id, &article, false, &[]).unwrap();
        let article_id = db::article_id_by_url(&conn, "https://example.com/old-unread")
            .unwrap()
            .unwrap();
        db::set_sync_state(&conn, article_id, true, false).unwrap();
        let mut remote_feed_ids = HashMap::new();
        remote_feed_ids.insert("feed/42".to_string(), feed_id);

        let item = unread_item("feed/42", "https://example.com/old-unread");
        let applied =
            apply_remote_item(&conn, &item, &remote_feed_ids, &HashSet::new(), true).unwrap();

        assert!(applied.matched);
        assert!(!applied.imported);
        assert!(!applied.state_changed);
        assert!(applied.pending_gained_remote_id);
        assert_eq!(db::count_unread(&conn).unwrap(), 0);
        let entries = db::take_sync_queue(&conn).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].remote_id, item.id);
        assert_eq!(entries[0].field, "read");
        assert!(entries[0].value);

        let applied_again =
            apply_remote_item(&conn, &item, &remote_feed_ids, &HashSet::new(), true).unwrap();
        assert!(applied_again.matched);
        assert!(applied_again.pending_gained_remote_id);
    }

    #[test]
    fn remote_unread_pending_local_edit_gains_remote_id_without_overwriting_state() {
        let (conn, _tmp) = test_conn();
        let feed_id = db::insert_feed(
            &conn,
            "https://example.com/feed.xml",
            Some("https://example.com"),
            "Example",
            None,
            SourceType::Rss,
            None,
        )
        .unwrap();
        let article = db::NewArticle {
            guid: "local-guid".to_string(),
            url: Some("https://example.com/old-unread".to_string()),
            title: "Local article".to_string(),
            author: None,
            summary: None,
            content_html: None,
            body_text: String::new(),
            image_url: None,
            published_at: None,
            enclosures: Vec::new(),
        };
        db::upsert_article(&conn, feed_id, &article, false, &[]).unwrap();
        let article_id = db::article_id_by_url(&conn, "https://example.com/old-unread")
            .unwrap()
            .unwrap();
        db::set_sync_state(&conn, article_id, true, false).unwrap();
        db::enqueue_sync(&conn, article_id, "read", true).unwrap();
        let mut remote_feed_ids = HashMap::new();
        remote_feed_ids.insert("feed/42".to_string(), feed_id);
        let pending: HashSet<i64> = db::pending_sync_article_ids(&conn)
            .unwrap()
            .into_iter()
            .collect();

        let item = unread_item("feed/42", "https://example.com/old-unread");
        let applied = apply_remote_item(&conn, &item, &remote_feed_ids, &pending, true).unwrap();

        assert!(applied.matched);
        assert!(applied.pending_gained_remote_id);
        assert!(!applied.state_changed);
        assert_eq!(db::count_unread(&conn).unwrap(), 0);
        let entries = db::take_sync_queue(&conn).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].remote_id, item.id);
        assert_eq!(entries[0].field, "read");
        assert!(entries[0].value);
    }

    #[test]
    fn remote_unread_stream_without_feed_mapping_does_not_import() {
        let (conn, _tmp) = test_conn();
        let item = unread_item("feed/42", "https://example.com/old-unread");
        let applied =
            apply_remote_item(&conn, &item, &HashMap::new(), &HashSet::new(), true).unwrap();

        assert!(!applied.imported);
        assert!(applied.skipped_no_feed);
        assert_eq!(db::count_unread(&conn).unwrap(), 0);
    }
}
