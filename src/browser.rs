use std::{
    env,
    io::{self, Write},
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use chromiumoxide::{
    Browser, browser::BrowserConfig, cdp::browser_protocol::input::InsertTextParams, layout::Point,
};
use directories::ProjectDirs;
use futures_util::StreamExt;
use serde::Serialize;
use tokio::time::{sleep, timeout};

use crate::data::{
    ClearDraftResult, ItemsResult, MessagesResult, PageSnapshot, SendMessageResult, UiItem,
    VisibleMessage,
};

const DEFAULT_TEAMS_URL: &str = "https://teams.live.com/v2/";
const LOGIN_WAIT: Duration = Duration::from_secs(10 * 60);
const MAX_VISIBLE_TEXT_CHARS: usize = 20_000;
const MAX_ITEMS: usize = 100;
const MAX_MESSAGES: usize = 100;
const MAX_OUTGOING_MESSAGE_CHARS: usize = 4_000;
const MAX_SCROLL_ATTEMPTS: usize = 12;
const FAILED_REPLY_TTL: Duration = Duration::from_secs(5 * 60);

static PROFILE_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
static LAST_FAILED_REPLY: OnceLock<Mutex<Option<FailedReply>>> = OnceLock::new();

#[derive(Debug, Clone)]
struct FailedReply {
    chat_name: String,
    message: String,
    failed_at: Instant,
}

fn last_failed_reply() -> &'static Mutex<Option<FailedReply>> {
    LAST_FAILED_REPLY.get_or_init(|| Mutex::new(None))
}

fn remember_failed_reply(chat_name: &str, message: &str) {
    if let Ok(mut failed_reply) = last_failed_reply().lock() {
        *failed_reply = Some(FailedReply {
            chat_name: chat_name.to_string(),
            message: message.to_string(),
            failed_at: Instant::now(),
        });
    }
}

fn clear_failed_reply() {
    if let Ok(mut failed_reply) = last_failed_reply().lock() {
        *failed_reply = None;
    }
}

fn reject_unapproved_standalone_fallback(
    chat_name: &str,
    message: &str,
    allow_after_reply_failure: bool,
) -> Result<()> {
    let Ok(mut failed_reply) = last_failed_reply().lock() else {
        return Ok(());
    };
    let Some(failed) = failed_reply.as_ref() else {
        return Ok(());
    };
    if failed.failed_at.elapsed() > FAILED_REPLY_TTL {
        *failed_reply = None;
        return Ok(());
    }
    if !allow_after_reply_failure && failed.chat_name == chat_name && failed.message == message {
        bail!(
            "A reply attempt for this exact message recently failed; refusing to send it as a standalone message. Set allow_standalone_after_reply_failure=true only when the user explicitly requests a new top-level message."
        );
    }
    Ok(())
}

struct BrowserSession {
    browser: Browser,
    handler_task: tokio::task::JoinHandle<()>,
    page: chromiumoxide::Page,
    owns_browser: bool,
    _profile_guard: tokio::sync::MutexGuard<'static, ()>,
}

fn profile_lock() -> &'static tokio::sync::Mutex<()> {
    PROFILE_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn app_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("io", "teams", "teams-mcp")
        .context("could not determine an OS application-data directory")
}

fn profile_dir() -> Result<PathBuf> {
    Ok(app_dirs()?.data_dir().join("browser-profile"))
}

fn teams_url() -> String {
    env::var("TEAMS_WEB_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_TEAMS_URL.to_string())
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn browser_config(headful: bool) -> Result<BrowserConfig> {
    let profile = profile_dir()?;
    std::fs::create_dir_all(&profile)
        .with_context(|| format!("could not create browser profile at {}", profile.display()))?;

    let mut builder = BrowserConfig::builder()
        .user_data_dir(profile)
        .port(0)
        .window_size(1440, 1000);
    if let Ok(executable) = env::var("TEAMS_BROWSER_EXECUTABLE") {
        builder = builder.chrome_executable(executable);
    }
    let headful = headful
        || env::var("TEAMS_HEADFUL")
            .map(|value| {
                !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "no"
                )
            })
            .unwrap_or(false);
    if headful {
        builder = builder.with_head();
    }
    builder
        .build()
        .map_err(|error| anyhow::anyhow!("could not build Chromium configuration: {error}"))
}

async fn launch(headful: bool) -> Result<(Browser, chromiumoxide::Handler)> {
    if let Ok(cdp_url) = env::var("TEAMS_CDP_URL") {
        if cdp_url.trim().is_empty() {
            bail!("TEAMS_CDP_URL is set but empty");
        }
        tracing::info!(%cdp_url, "attaching to an existing browser over CDP");
        return Browser::connect(cdp_url).await.map_err(Into::into);
    }

    Browser::launch(browser_config(headful)?).await.map_err(|error| {
        anyhow::anyhow!(
            "could not launch Chromium: {error}. Install Chrome/Chromium or set TEAMS_CDP_URL to an existing browser WebSocket URL"
        )
    })
}

fn is_retryable_page_context_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_lowercase();
    message.contains("cannot find context with specified id")
        || message.contains("execution context was destroyed")
        || message.contains("cannot find execution context")
}

async fn page_snapshot(page: &chromiumoxide::Page) -> Result<PageSnapshot> {
    for attempt in 0..40 {
        let snapshot = async {
            let page_title = page.get_title().await?.unwrap_or_default();
            let url = page.url().await?.unwrap_or_default();
            let state: serde_json::Value = page
                .evaluate(
                    r#"() => {
                        const app = document.querySelector('#app');
                        const error = document.querySelector('#error-screen');
                        const loading = document.querySelector('#loading-screen');
                        const config = document.head?.getAttribute('data-config') || '';
                        const text = (app?.innerText || document.body?.innerText || '').trim();
                        return {
                            visible_text: text,
                            unauthenticated: config.includes('"unauthenticated":true'),
                            error_visible: !!error && getComputedStyle(error).display !== 'none' && error.classList.contains('show'),
                            loading_visible: !!loading && getComputedStyle(loading).display !== 'none',
                            app_children: app?.childElementCount || 0
                        };
                    }"#,
                )
                .await?
                .into_value()?;
            let visible_text = state
                .get("visible_text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let visible_text = truncate(visible_text, MAX_VISIBLE_TEXT_CHARS);
            let app_children = state
                .get("app_children")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default();
            let unauthenticated = state
                .get("unauthenticated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let error_visible = state
                .get("error_visible")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let loading_visible = state
                .get("loading_visible")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            Ok::<PageSnapshot, anyhow::Error>(PageSnapshot {
                authenticated: is_authenticated_url(&url)
                    && !looks_like_login_page(&visible_text)
                    && !unauthenticated
                    && !error_visible
                    && !loading_visible
                    && app_children > 0,
                page_title,
                url,
                visible_text,
            })
        }
        .await;

        match snapshot {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) if is_retryable_page_context_error(&error) && attempt < 39 => {
                sleep(Duration::from_millis(250)).await;
            }
            Err(error) => return Err(error),
        }
    }

    bail!("the Teams page did not expose a stable browser execution context")
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn validate_outgoing_message(message: &str) -> Result<()> {
    if message.trim().is_empty() {
        bail!("message is required");
    }
    if message.chars().count() > MAX_OUTGOING_MESSAGE_CHARS {
        bail!("message is limited to {MAX_OUTGOING_MESSAGE_CHARS} characters");
    }
    if message.contains(['\n', '\r']) {
        bail!("message must be a single line for the initial send implementation");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MentionToken {
    start: usize,
    end: usize,
    query: String,
}

fn mention_tokens(message: &str) -> Vec<MentionToken> {
    let mut tokens = Vec::new();
    for (start, character) in message.char_indices() {
        if character != '@' {
            continue;
        }
        let previous = message[..start].chars().next_back();
        if previous.is_some_and(|value| value.is_alphanumeric() || "._-".contains(value)) {
            continue;
        }
        let end = message[start + character.len_utf8()..]
            .char_indices()
            .find(|(_, value)| !value.is_alphanumeric() && !"._-".contains(*value))
            .map(|(offset, _)| start + character.len_utf8() + offset)
            .unwrap_or(message.len());
        let query = &message[start + character.len_utf8()..end];
        if query.chars().any(char::is_alphanumeric) {
            tokens.push(MentionToken {
                start,
                end,
                query: query.to_string(),
            });
        }
    }
    tokens
}

fn person_key(value: &str) -> String {
    value
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_alphanumeric())
        .collect()
}

fn resolve_mention_names(tokens: &[MentionToken], candidates: &[String]) -> Result<Vec<String>> {
    let mut resolved = Vec::with_capacity(tokens.len());
    for token in tokens {
        let wanted = person_key(&token.query);
        let matches: Vec<&String> = candidates
            .iter()
            .filter(|candidate| {
                let key = person_key(candidate);
                key == wanted || (wanted.len() >= 3 && key.starts_with(&wanted))
            })
            .collect();
        match matches.as_slice() {
            [candidate] => resolved.push((*candidate).clone()),
            [] => bail!(
                "Could not resolve @{0} to a visible Teams participant; refusing to send a literal @text mention",
                token.query
            ),
            _ => bail!(
                "@{0} matches multiple visible Teams participants; use a more specific name",
                token.query
            ),
        }
    }
    Ok(resolved)
}

fn looks_like_login_page(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("sign in")
        || text.contains("log in")
        || text.contains("enter password")
        || text.contains("enter your password")
}

fn is_authenticated_url(url: &str) -> bool {
    let url = url.to_lowercase();
    (url.contains("teams.cloud.microsoft")
        || url.contains("teams.microsoft.com")
        || url.contains("teams.live.com"))
        && !url.contains("login.microsoftonline.com")
        && !url.contains("/error/")
}

async fn wait_for_teams(page: &chromiumoxide::Page) -> Result<PageSnapshot> {
    let mut snapshot = page_snapshot(page).await?;
    for _ in 0..40 {
        if snapshot.authenticated {
            return Ok(snapshot);
        }
        sleep(Duration::from_millis(500)).await;
        snapshot = page_snapshot(page).await?;
    }
    Ok(snapshot)
}

async fn open_teams(headful: bool) -> Result<BrowserSession> {
    let profile_guard = profile_lock().lock().await;
    let owns_browser = env::var_os("TEAMS_CDP_URL").is_none();
    let (mut browser, mut handler) = launch(headful).await?;
    let handler_task = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if let Err(error) = event {
                tracing::debug!(%error, "browser handler stopped");
                break;
            }
        }
    });

    let page = if owns_browser {
        browser.new_page(teams_url()).await?
    } else {
        browser.fetch_targets().await?;
        sleep(Duration::from_millis(250)).await;
        browser
            .pages()
            .await?
            .into_iter()
            .next()
            .context("connected browser has no open pages")?
    };
    Ok(BrowserSession {
        browser,
        handler_task,
        page,
        owns_browser,
        _profile_guard: profile_guard,
    })
}

async fn finish(mut session: BrowserSession) -> Result<()> {
    if session.owns_browser {
        session.browser.close().await?;
        session.browser.wait().await?;
    }
    session.handler_task.abort();
    Ok(())
}

async fn prepared_page() -> Result<BrowserSession> {
    let session = open_teams(false).await?;
    if session.owns_browser {
        session.page.goto(teams_url()).await?;
    }
    let snapshot = wait_for_teams(&session.page).await?;
    if !snapshot.authenticated {
        finish(session).await?;
        bail!("Teams is not authenticated; run `teams-mcp login` first");
    }
    Ok(session)
}

async fn evaluate_items(page: &chromiumoxide::Page, kind: &str) -> Result<Vec<UiItem>> {
    let kind = serde_json::to_string(kind)?;
    page.evaluate(format!(
        r#"() => {{
            const kind = {kind};
            const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
            const seen = new Set();
            const result = [];
            const add = (element, label, secondary = null, id = null) => {{
                label = normalize(label);
                secondary = normalize(secondary);
                if (!label || label.length > 160 || seen.has(label)) return;
                if (label.length < 2) return;
                seen.add(label);
                result.push({{
                    label,
                    id: id || element?.getAttribute('data-item-id') || element?.getAttribute('data-id') || element?.getAttribute('data-tid') || null,
                    secondary_label: secondary || null,
                    aria_label: element?.getAttribute('aria-label') || null
                }});
            }};
            if (kind === 'chats') {{
                const roots = Array.from(document.querySelectorAll('[data-tid="app-layout-area--mid-nav"]'));
                const rows = roots.flatMap(root => Array.from(root.querySelectorAll('[role="treeitem"]')))
                    .filter(element => element.querySelector('[id^="title-chat-list-item_"]'));
                const fallbackRows = Array.from(document.querySelectorAll('[id^="title-chat-list-item_"]'))
                    .map(title => title.closest('[role="treeitem"]'))
                    .filter(Boolean);
                const uniqueRows = Array.from(new Set(rows.length ? rows : fallbackRows));
                uniqueRows.forEach(element => {{
                    const title = element.querySelector('[id^="title-chat-list-item_"]');
                    const label = normalize(title?.innerText || '');
                    if (!label || /\(you\)$/i.test(label)) return;
                    const time = element.querySelector('[id^="time-chat-list-item_"]');
                    const secondary = normalize(time?.innerText || time?.textContent || '');
                    add(element, label, secondary, title?.id || null);
                }});
                return result.slice(0, {MAX_ITEMS});
            }}
            const selectors = kind === 'teams' || kind === 'channels'
                ? '[role="treeitem"], [data-tid*="team" i], [data-tid*="channel" i], [aria-label*="team" i], [aria-label*="channel" i]'
                : '[role="listitem"], [role="treeitem"], [data-tid*="chat" i], [aria-label*="chat" i]';
            document.querySelectorAll(selectors).forEach(element => {{
                const aria = element.getAttribute('aria-label') || '';
                const dataTid = element.getAttribute('data-tid') || '';
                const text = element.innerText || element.textContent || '';
                const context = `${{aria}} ${{dataTid}}`;
                if (kind === 'teams' && (element.matches('[role="treeitem"]') || /team/i.test(context))) add(element, text, aria);
                if (kind === 'channels' && (element.matches('[role="treeitem"]') || /channel|general/i.test(context))) add(element, text, aria);
                if (kind === 'chats' && (element.matches('[role="listitem"], [role="treeitem"]') || /chat/i.test(context))) add(element, text, aria);
            }});
            return result.slice(0, {MAX_ITEMS});
        }}"#
    ))
    .await?
    .into_value()
    .map_err(Into::into)
}

async fn click_navigation(page: &chromiumoxide::Page, labels: &[&str]) -> Result<bool> {
    let labels = serde_json::to_string(labels)?;
    page.evaluate(format!(
        r#"() => {{
            const wanted = {labels}.map(value => value.toLowerCase());
            const normalize = value => (value || '').replace(/\s+/g, ' ').trim().toLowerCase();
            const candidates = Array.from(document.querySelectorAll('button, [role="tab"], [role="link"], a'));
            const target = candidates.find(element => {{
                const values = [element.getAttribute('aria-label'), element.innerText, element.textContent]
                    .map(normalize)
                    .filter(Boolean);
                return values.some(value => wanted.includes(value) || wanted.some(label => value === label || value.startsWith(`${{label}} `)));
            }});
            if (!target) return false;
            target.click();
            return true;
        }}"#
    ))
    .await?
    .into_value()
    .map_err(Into::into)
}

async fn click_visible_label(page: &chromiumoxide::Page, label: &str) -> Result<bool> {
    let label = serde_json::to_string(label)?;
    page.evaluate(format!(
        r#"() => {{
            const wanted = {label};
            const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
            const title = Array.from(document.querySelectorAll('[data-tid="chat-title"], [id^="title-chat-list-item_"]'))
                .find(element => normalize(element.innerText || element.textContent) === wanted);
            const candidates = Array.from(document.querySelectorAll('button, [role="treeitem"], [role="listitem"], a'));
            const target = title?.closest('[role="treeitem"]') || title || candidates.find(element => normalize(element.innerText || element.textContent) === wanted || element.getAttribute('aria-label') === wanted);
            if (!target) return false;
            target.click();
            return true;
        }}"#
    ))
    .await?
    .into_value()
    .map_err(Into::into)
}

async fn scroll_message_list_to_older(page: &chromiumoxide::Page) -> Result<bool> {
    page.evaluate(
        r#"() => {
            const candidates = Array.from(document.querySelectorAll('*')).filter(element => {
                const name = `${element.getAttribute('aria-label') || ''} ${element.getAttribute('data-tid') || ''} ${element.className || ''}`;
                return /message|chat|conversation/i.test(name) && element.scrollHeight > element.clientHeight + 40;
            });
            const container = candidates.sort((a, b) => b.scrollHeight - a.scrollHeight)[0];
            if (!container) return false;
            const before = container.scrollTop;
            container.scrollTop = 0;
            container.dispatchEvent(new Event('scroll', { bubbles: true }));
            return before > 0;
        }"#,
    )
    .await?
    .into_value()
    .map_err(Into::into)
}

async fn evaluate_messages(
    page: &chromiumoxide::Page,
    page_number: u32,
    page_size: usize,
) -> Result<(Vec<VisibleMessage>, bool)> {
    page.evaluate(format!(
        r#"() => {{
            const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
            const seen = new Set();
            const all = [];
            const candidates = Array.from(document.querySelectorAll('[data-tid="chat-pane-message"], [data-tid="message-body"]'));
            candidates.forEach(element => {{
                const container = element.closest('[data-tid="chat-pane-item"]') || element;
                const text = normalize(element.getAttribute('aria-label') || element.innerText || element.textContent);
                if (!text) return;
                const messageId = element.getAttribute('data-message-id') || element.getAttribute('data-id') || element.getAttribute('id');
                const key = messageId || text;
                if (seen.has(key)) return;
                seen.add(key);
                const authorNode = container.querySelector('[data-tid="message-author-name"], [data-tid*="author" i], [data-tid*="sender" i], [class*="author" i], [class*="sender" i]');
                const timeNode = container.querySelector('time, [data-tid*="time" i], [class*="time" i]');
                const author = normalize(authorNode?.innerText || authorNode?.getAttribute('aria-label'));
                const timestamp = normalize(timeNode?.getAttribute('datetime') || timeNode?.innerText || timeNode?.getAttribute('aria-label'));
                const authorId = authorNode?.getAttribute('data-user-id') || authorNode?.getAttribute('data-id') || null;
                const edited = /edited|redigert/i.test(container.innerText || '');
                all.push({{
                    text,
                    message_id: messageId || null,
                    author: author || null,
                    author_id: authorId,
                    timestamp: timestamp || null,
                    edited: edited ? true : null
                }});
            }});
            const page = {page_number};
            const size = {page_size};
            const end = all.length - ((page - 1) * size);
            const start = Math.max(0, end - size);
            return [all.slice(Math.max(0, start), Math.max(0, end)), start > 0];
        }}"#
    ))
    .await?
    .into_value()
    .map_err(Into::into)
}

async fn evaluate_members(page: &chromiumoxide::Page) -> Result<Vec<UiItem>> {
    page.evaluate(format!(
        r#"() => {{
            const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
            const seen = new Set();
            const selectors = '[role="listitem"], [role="option"], [role="menuitem"], [data-tid*="member" i], [data-tid*="participant" i], [data-tid*="person" i], [aria-label*="member" i], [aria-label*="participant" i], [class*="member" i], [class*="participant" i]';
            return Array.from(document.querySelectorAll(selectors))
                .map(element => {{
                    const raw = element.getAttribute('aria-label') || element.innerText || element.textContent || '';
                    const lines = raw.split(/\n+/).map(normalize).filter(Boolean);
                    const label = lines[0] || '';
                    return {{
                        label,
                        id: element.getAttribute('data-user-id') || element.getAttribute('data-id') || null,
                        secondary_label: lines.slice(1).join(' ') || normalize(element.getAttribute('title')) || null,
                        aria_label: element.getAttribute('aria-label') || null
                    }};
                }})
                .filter(item => item.label && item.label.length >= 2 && item.label.length <= 160 && !/^people(?: \(\d+\))?$/i.test(item.label) && !/^add people$/i.test(item.label) && !/chat participants|view and add participants/i.test(item.label) && !seen.has(item.label) && seen.add(item.label))
                .slice(0, {MAX_ITEMS});
        }}"#
    ))
    .await?
    .into_value()
    .map_err(Into::into)
}

pub async fn visible_snapshot() -> Result<PageSnapshot> {
    let session = prepared_page().await?;
    let result = page_snapshot(&session.page).await;
    finish(session).await?;
    result
}

pub async fn list_teams() -> Result<ItemsResult> {
    let session = prepared_page().await?;
    let result = async {
        let clicked = click_navigation(&session.page, &["Teams", "Team"]).await?;
        if clicked {
            sleep(Duration::from_millis(500)).await;
        }
        let mut warnings = Vec::new();
        if !clicked {
            warnings.push("Could not find the Teams navigation control; no team labels were returned from the current chat page.".to_string());
        }
        let items = if clicked {
            evaluate_items(&session.page, "teams").await?
        } else {
            Vec::new()
        };
        if items.is_empty() {
            warnings.push("The current Teams UI did not expose team labels in the visible DOM; use inspect to capture the current page shape.".to_string());
        }
        Ok(ItemsResult {
            page_url: session.page.url().await?.unwrap_or_default(),
            page: 1,
            page_size: items.len() as u32,
            has_more: false,
            items,
            warnings,
        })
    }
    .await;
    finish(session).await?;
    result
}

pub async fn list_channels(team_name: Option<&str>) -> Result<ItemsResult> {
    let session = prepared_page().await?;
    let result = async {
        let navigation_clicked = click_navigation(&session.page, &["Teams", "Team"]).await?;
        if navigation_clicked {
            sleep(Duration::from_millis(400)).await;
        }
        let mut warnings = Vec::new();
        if !navigation_clicked {
            warnings.push("Could not find the Teams navigation control; channels were read from the current page instead.".to_string());
        }
        if let Some(team_name) = team_name.filter(|value| !value.trim().is_empty()) {
            let clicked = click_visible_label(&session.page, team_name).await?;
            if clicked {
                sleep(Duration::from_millis(400)).await;
            } else {
                warnings.push(format!("Could not find an exact visible team label {team_name:?}; channels were read from the current page instead."));
            }
        }
        let items = if navigation_clicked {
            evaluate_items(&session.page, "channels").await?
        } else {
            Vec::new()
        };
        if items.is_empty() {
            warnings.push("The current Teams UI did not expose channel labels in the visible DOM.".to_string());
        }
        Ok(ItemsResult {
            page_url: session.page.url().await?.unwrap_or_default(),
            page: 1,
            page_size: items.len() as u32,
            has_more: false,
            items,
            warnings,
        })
    }
    .await;
    finish(session).await?;
    result
}

pub async fn list_chats() -> Result<ItemsResult> {
    let session = prepared_page().await?;
    let result = async {
        let clicked = click_navigation(&session.page, &["Chat", "Chats"]).await?;
        if clicked {
            sleep(Duration::from_millis(500)).await;
        }
        let items = evaluate_items(&session.page, "chats").await?;
        let mut warnings = Vec::new();
        if !clicked {
            warnings.push("Could not find the Chat navigation control; labels were read from the current page instead.".to_string());
        }
        if items.is_empty() {
            warnings.push("The current Teams UI did not expose chat labels in the visible DOM.".to_string());
        }
        Ok(ItemsResult {
            page_url: session.page.url().await?.unwrap_or_default(),
            page: 1,
            page_size: items.len() as u32,
            has_more: false,
            items,
            warnings,
        })
    }
    .await;
    finish(session).await?;
    result
}

pub async fn list_chat_members(chat_name: &str) -> Result<ItemsResult> {
    if chat_name.trim().is_empty() {
        bail!("chat_name is required");
    }
    let session = prepared_page().await?;
    let result = async {
        let navigation_clicked = click_navigation(&session.page, &["Chat", "Chats"]).await?;
        if navigation_clicked {
            sleep(Duration::from_millis(400)).await;
        }
        let clicked = click_visible_label(&session.page, chat_name).await?;
        if clicked {
            sleep(Duration::from_millis(600)).await;
        }
        let mut warnings = Vec::new();
        if !navigation_clicked {
            warnings.push("Could not find the Chat navigation control; members were read from the current page instead.".to_string());
        }
        if !clicked {
            warnings.push(format!("Could not find an exact visible chat label {chat_name:?}; members were read from the current page instead."));
        }
        let participant_button_clicked: bool = session
            .page
            .evaluate(
                r#"() => {
                    const button = document.querySelector('[data-tid="chat-header-participant-count"]');
                    if (!button) return false;
                    button.click();
                    return true;
                }"#,
            )
            .await?
            .into_value()?;
        if participant_button_clicked {
            sleep(Duration::from_millis(500)).await;
        }
        let items = evaluate_members(&session.page).await?;
        if !participant_button_clicked {
            warnings.push("The current Teams UI did not expose the chat participant control.".to_string());
        }
        if items.is_empty() {
            warnings.push("The current Teams UI did not expose chat-member elements in the visible DOM.".to_string());
        }
        Ok(ItemsResult {
            page_url: session.page.url().await?.unwrap_or_default(),
            page: 1,
            page_size: items.len() as u32,
            has_more: false,
            items,
            warnings,
        })
    }
    .await;
    finish(session).await?;
    result
}

pub async fn visible_messages(
    chat_name: &str,
    page_number: u32,
    page_size: u32,
) -> Result<MessagesResult> {
    if chat_name.trim().is_empty() {
        bail!("chat_name is required");
    }
    if page_number == 0 {
        bail!("page must be at least 1");
    }
    let page_size = page_size.clamp(1, MAX_MESSAGES as u32) as usize;
    let session = prepared_page().await?;
    let result = async {
        let navigation_clicked = click_navigation(&session.page, &["Chat", "Chats"]).await?;
        if navigation_clicked {
            sleep(Duration::from_millis(400)).await;
        }
        let clicked = click_visible_label(&session.page, chat_name).await?;
        if clicked {
            sleep(Duration::from_millis(600)).await;
        }
        let mut warnings = Vec::new();
        if !navigation_clicked {
            warnings.push("Could not find the Chat navigation control; messages were read from the current page instead.".to_string());
        }
        if !clicked {
            warnings.push(format!("Could not find an exact visible chat label {chat_name:?}; messages were read from the current page instead."));
        }
        let mut reached_top = page_number == 1;
        for _ in 1..page_number.min(MAX_SCROLL_ATTEMPTS as u32) {
            let moved = scroll_message_list_to_older(&session.page).await?;
            if !moved {
                reached_top = true;
                break;
            }
            reached_top = false;
            sleep(Duration::from_millis(500)).await;
        }
        if page_number > MAX_SCROLL_ATTEMPTS as u32 {
            warnings.push(format!("Message pagination scrolls are capped at {MAX_SCROLL_ATTEMPTS} pages per call."));
        }
        let (messages, mut has_more) =
            evaluate_messages(&session.page, page_number, page_size).await?;
        has_more |= !reached_top;
        if messages.is_empty() {
            warnings.push("The current Teams UI did not expose message elements in the visible DOM.".to_string());
        }
        Ok(MessagesResult {
            page_url: session.page.url().await?.unwrap_or_default(),
            page: page_number,
            page_size: page_size as u32,
            has_more,
            messages,
            warnings,
        })
    }
    .await;
    finish(session).await?;
    result
}

const COMPOSER_SELECTOR: &str = r#"[data-tid="ckeditor"][role="textbox"], [role="textbox"][aria-label="Type a message"], [contenteditable="true"][aria-label="Type a message"]"#;
const SEND_SELECTOR: &str = r#"[data-tid="newMessageCommands-send"], button[aria-label^="Send"]"#;

async fn composer_state(page: &chromiumoxide::Page) -> Result<(bool, String)> {
    let state: serde_json::Value = page
        .evaluate(format!(
            r#"() => {{
                const box = document.querySelector({composer:?});
                return {{
                    found: !!box,
                    text: box?.innerText || box?.textContent || ''
                }};
            }}"#,
            composer = COMPOSER_SELECTOR,
        ))
        .await?
        .into_value()?;
    Ok((
        state
            .get("found")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        state
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
    ))
}

async fn clear_composer(page: &chromiumoxide::Page) -> Result<bool> {
    let (found, text) = composer_state(page).await?;
    if !found {
        bail!("The current Teams UI did not expose the message composer");
    }
    if text.trim().is_empty() {
        return Ok(false);
    }

    let composer = page
        .find_element(COMPOSER_SELECTOR)
        .await
        .context("could not locate the Teams message composer")?;
    composer.click().await?;
    page.evaluate(format!(
        r#"() => {{
            const box = document.querySelector({composer:?});
            if (!box) return false;
            const selection = window.getSelection();
            const range = document.createRange();
            range.selectNodeContents(box);
            selection?.removeAllRanges();
            selection?.addRange(range);
            return true;
        }}"#,
        composer = COMPOSER_SELECTOR,
    ))
    .await?
    .into_value::<bool>()?;
    composer.press_key("Backspace").await?;
    sleep(Duration::from_millis(150)).await;

    let (_, remaining) = composer_state(page).await?;
    if !remaining.trim().is_empty() {
        bail!("Teams did not clear the message composer draft");
    }
    Ok(true)
}

async fn chat_participant_names(page: &chromiumoxide::Page) -> Result<Vec<String>> {
    let mut names = page
        .evaluate(
            r#"() => Array.from(document.querySelectorAll('[data-tid="message-author-name"]'))
                .map(element => (element.innerText || element.textContent || '').replace(/\s+/g, ' ').trim())
                .filter(Boolean)"#,
        )
        .await?
        .into_value::<Vec<String>>()?;

    let participant_button_clicked: bool = page
        .evaluate(
            r#"() => {
                const button = document.querySelector('[data-tid="chat-header-participant-count"], [aria-label*="participant" i]');
                if (!button) return false;
                button.click();
                return true;
            }"#,
        )
        .await?
        .into_value()?;
    if participant_button_clicked {
        sleep(Duration::from_millis(400)).await;
        for item in evaluate_members(page).await? {
            names.push(item.label);
        }
        // Leave the composer unobstructed and ensure the mention picker is the
        // only participant-like popup open when a token is typed.
        page.evaluate(
            r#"() => {
                const button = document.querySelector('[data-tid="chat-header-participant-count"], [aria-label*="participant" i]');
                button?.click();
            }"#,
        )
        .await?;
        sleep(Duration::from_millis(150)).await;
    }

    let mut unique = Vec::new();
    for name in names {
        if !name.is_empty() && !unique.iter().any(|item| item == &name) {
            unique.push(name);
        }
    }
    Ok(unique)
}

async fn click_mention_suggestion(page: &chromiumoxide::Page, display_name: &str) -> Result<bool> {
    let wanted = serde_json::to_string(&person_key(display_name))?;
    page.evaluate(format!(
        r#"() => {{
            const wanted = {wanted};
            const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
            const key = value => Array.from(normalize(value).toLowerCase())
                .filter(character => /[a-z0-9\u00c0-\u024f]/i.test(character)).join('');
            const visible = element => {{
                const rect = element.getBoundingClientRect();
                const style = getComputedStyle(element);
                return style.display !== 'none' && style.visibility !== 'hidden' && rect.width > 0 && rect.height > 0;
            }};
            const label = element => normalize(element.getAttribute('aria-label') || element.innerText || element.textContent);
            const candidates = Array.from(document.querySelectorAll(
                '[role="option"], [role="menuitem"], [data-tid*="mention" i], [data-tid*="suggest" i], [data-tid*="person" i]'
            )).filter(visible);
            const exact = candidates.find(element => key(label(element)) === wanted);
            const partial = candidates.find(element => key(label(element)).startsWith(wanted));
            const target = exact || partial;
            if (!target) return false;
            target.click();
            return true;
        }}"#,
        wanted = wanted,
    ))
    .await?
    .into_value()
    .map_err(Into::into)
}

async fn type_message_with_mentions(
    page: &chromiumoxide::Page,
    message: &str,
    tokens: &[MentionToken],
    resolved_mentions: &[String],
) -> Result<String> {
    let mut cursor = 0;
    let mut expected = String::new();
    for (token, display_name) in tokens.iter().zip(resolved_mentions) {
        let plain = &message[cursor..token.start];
        if !plain.is_empty() {
            page.execute(InsertTextParams::new(plain.to_string()))
                .await?;
            expected.push_str(plain);
        }

        let first_word = display_name
            .split_whitespace()
            .next()
            .unwrap_or(display_name);
        page.execute(InsertTextParams::new(format!("@{first_word}")))
            .await?;
        sleep(Duration::from_millis(350)).await;
        if !click_mention_suggestion(page, display_name).await? {
            bail!(
                "Teams did not expose a mention suggestion for {display_name:?}; refusing to send a literal @text mention"
            );
        }
        expected.push_str(display_name);
        cursor = token.end;
    }
    let suffix = &message[cursor..];
    if !suffix.is_empty() {
        page.execute(InsertTextParams::new(suffix.to_string()))
            .await?;
    }
    expected.push_str(suffix);
    Ok(expected)
}

async fn composer_has_reply_context(
    page: &chromiumoxide::Page,
    message_id: &str,
    target_text: &str,
) -> Result<bool> {
    let message_id = serde_json::to_string(message_id)?;
    let target_text = serde_json::to_string(&target_text.replace('\n', " "))?;
    page.evaluate(format!(
        r#"() => {{
            const wantedId = {message_id};
            const wantedText = {target_text}.replace(/\s+/g, ' ').trim().toLowerCase();
            const composer = document.querySelector({composer:?});
            if (!composer) return false;
            const composeFooter = composer.closest('[data-tid="chat-pane-compose-message-footer"]');
            const visible = element => {{
                const rect = element.getBoundingClientRect();
                const style = getComputedStyle(element);
                return style.display !== 'none' && style.visibility !== 'hidden' && rect.width > 0 && rect.height > 0;
            }};
            const inMessageList = element => element.closest('[data-tid="chat-pane-item"], [data-tid="message-pane-list-runway"]');
            const contexts = Array.from(document.querySelectorAll(
                '[data-tid="quoted-reply-card"], [data-tid*="quoted-reply" i], [data-tid*="reply" i], [data-tid*="quote" i], [aria-label*="reply" i], [aria-label*="quote" i]'
            )).filter(element => visible(element) && !inMessageList(element));
            const matchesTarget = element => {{
                const attributes = [element.id, element.getAttribute('data-message-id'), element.getAttribute('data-id'), element.getAttribute('aria-label')].filter(Boolean);
                const text = (element.innerText || element.textContent || '').replace(/\s+/g, ' ').trim().toLowerCase();
                return attributes.includes(wantedId) || (wantedText && text.includes(wantedText));
            }};
            if (contexts.some(matchesTarget)) return true;
            // Some Teams builds render the reply card in a portal without the
            // target message text; the compose footer is still an unambiguous scope.
            return !!composeFooter && contexts.some(element => composeFooter.contains(element));
        }}"#,
        composer = COMPOSER_SELECTOR,
        message_id = message_id,
        target_text = target_text,
    ))
    .await?
    .into_value()
    .map_err(Into::into)
}

async fn wait_for_reply_context(
    page: &chromiumoxide::Page,
    message_id: &str,
    target_text: &str,
) -> Result<bool> {
    for _ in 0..10 {
        if composer_has_reply_context(page, message_id, target_text).await? {
            return Ok(true);
        }
        sleep(Duration::from_millis(200)).await;
    }
    Ok(false)
}

async fn verify_sent_reply(
    page: &chromiumoxide::Page,
    message_id: &str,
    target_text: &str,
    outgoing_message: &str,
) -> Result<bool> {
    let message_id = serde_json::to_string(message_id)?;
    let target_text = serde_json::to_string(&target_text.replace('\n', " "))?;
    let outgoing_message = serde_json::to_string(outgoing_message)?;
    for _ in 0..10 {
        let verified: bool = page
            .evaluate(format!(
                r#"() => {{
                    const wantedId = {message_id};
                    const wantedText = {target_text}.replace(/\s+/g, ' ').trim().toLowerCase();
                    const outgoing = {outgoing_message}.replace(/\s+/g, ' ').trim().toLowerCase();
                    return Array.from(document.querySelectorAll('[data-tid="chat-pane-item"]')).some(item => {{
                        const card = item.querySelector('[data-tid="quoted-reply-card"], [data-tid*="quoted-reply" i], [data-tid*="reply-card" i]');
                        const body = item.querySelector('[data-tid="chat-pane-message"]');
                        if (!card || !body) return false;
                        const cardText = (card.innerText || card.textContent || '').replace(/\s+/g, ' ').trim().toLowerCase();
                        const bodyText = (body.innerText || body.textContent || '').replace(/\s+/g, ' ').trim().toLowerCase();
                        const attributes = [card.id, card.getAttribute('data-message-id'), card.getAttribute('data-id')].filter(Boolean);
                        return (attributes.includes(wantedId) || (wantedText && cardText.includes(wantedText))) && bodyText.includes(outgoing);
                    }});
                }}"#,
                message_id = message_id,
                target_text = target_text,
                outgoing_message = outgoing_message,
            ))
            .await?
            .into_value()?;
        if verified {
            return Ok(true);
        }
        sleep(Duration::from_millis(250)).await;
    }
    Ok(false)
}

async fn send_from_composer(
    page: &chromiumoxide::Page,
    chat_name: &str,
    message: &str,
    reply_to_message_id: Option<String>,
    reply_context_allowed: bool,
    reply_target: Option<(&str, &str)>,
    clear_draft: bool,
) -> Result<SendMessageResult> {
    let (found, initial_text) = composer_state(page).await?;
    if !found {
        bail!("The current Teams UI did not expose the message composer");
    }
    if !initial_text.trim().is_empty() {
        if !clear_draft {
            bail!("The message composer already contains a draft; refusing to overwrite it");
        }
        clear_composer(page).await?;
    }
    let cleanup_on_error = clear_draft || initial_text.trim().is_empty();

    let tokens = mention_tokens(message);
    let resolved_mentions = if tokens.is_empty() {
        Vec::new()
    } else {
        let candidates = chat_participant_names(page).await?;
        resolve_mention_names(&tokens, &candidates)?
    };

    let composer = page
        .find_element(COMPOSER_SELECTOR)
        .await
        .context("could not locate the Teams message composer")?;
    composer.click().await?;

    // type_str() calls press_key() for every character and fails for characters
    // that are not entries in chromiumoxide's keyboard-key table. Input.insertText
    // is the CDP operation intended for IME, emoji, and other Unicode text. Mention
    // tokens are typed through Teams' picker so they become real mention entities.
    let expected_text =
        match type_message_with_mentions(page, message, &tokens, &resolved_mentions).await {
            Ok(text) => text,
            Err(error) => {
                if cleanup_on_error {
                    let _ = clear_composer(page).await;
                }
                return Err(error);
            }
        };
    sleep(Duration::from_millis(150)).await;

    let typed: serde_json::Value = match page
        .evaluate(format!(
            r#"() => {{
                const box = document.querySelector({composer:?});
                const send = document.querySelector({send:?});
                return {{
                    text: box?.innerText || box?.textContent || '',
                    disabled: !send || send.disabled || send.getAttribute('aria-disabled') === 'true'
                }};
            }}"#,
            composer = COMPOSER_SELECTOR,
            send = SEND_SELECTOR,
        ))
        .await
    {
        Ok(value) => match value.into_value() {
            Ok(value) => value,
            Err(error) => {
                if cleanup_on_error {
                    let _ = clear_composer(page).await;
                }
                return Err(error.into());
            }
        },
        Err(error) => {
            if cleanup_on_error {
                let _ = clear_composer(page).await;
            }
            return Err(error.into());
        }
    };
    let typed_text = typed
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let typed_text_matches = typed_text.trim() == expected_text.trim()
        || (reply_context_allowed && typed_text.trim_end().ends_with(expected_text.trim()));
    if !typed_text_matches {
        if cleanup_on_error {
            let _ = clear_composer(page).await;
        }
        bail!("Teams did not expose the typed message exactly; refusing to click Send");
    }
    if typed
        .get("disabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
    {
        if cleanup_on_error {
            let _ = clear_composer(page).await;
        }
        bail!("The Teams Send control is unavailable or disabled");
    }

    let send_button = match page
        .find_element(SEND_SELECTOR)
        .await
        .context("could not locate the Teams Send control")
    {
        Ok(button) => button,
        Err(error) => {
            if cleanup_on_error {
                let _ = clear_composer(page).await;
            }
            return Err(error);
        }
    };
    if let Err(error) = send_button.click().await {
        if cleanup_on_error {
            let _ = clear_composer(page).await;
        }
        return Err(error.into());
    }
    sleep(Duration::from_millis(700)).await;

    let remaining: String = page
        .evaluate(format!(
            r#"() => document.querySelector({composer:?})?.innerText || document.querySelector({composer:?})?.textContent || ''"#,
            composer = COMPOSER_SELECTOR,
        ))
        .await?
        .into_value()?;
    let mut warnings = Vec::new();
    if !remaining.trim().is_empty() {
        warnings.push(
            "Teams did not clear the composer after clicking Send; verify delivery in the chat."
                .to_string(),
        );
    }
    let reply_context_verified = if let Some((message_id, target_text)) = reply_target {
        let verified = verify_sent_reply(page, message_id, target_text, typed_text).await?;
        if !verified {
            warnings.push(
                "Teams sent the message, but the target quoted-reply card could not be verified; inspect the chat before retrying.".to_string(),
            );
        }
        Some(verified)
    } else {
        None
    };
    Ok(SendMessageResult {
        chat_name: chat_name.to_string(),
        message: message.to_string(),
        reply_to_message_id,
        sent: true,
        confirmation_required: false,
        page_url: page.url().await?.unwrap_or_default(),
        resolved_mentions,
        reply_context_verified,
        warnings,
    })
}

async fn ensure_composer_empty(page: &chromiumoxide::Page) -> Result<()> {
    let (found, text) = composer_state(page).await?;
    if !found {
        bail!("The current Teams UI did not expose the message composer");
    }
    if !text.trim().is_empty() {
        bail!("The message composer already contains a draft; refusing to overwrite it");
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct MessageTarget {
    point: Point,
    text: String,
}

async fn message_center(
    page: &chromiumoxide::Page,
    message_id: &str,
) -> Result<Option<MessageTarget>> {
    let message_id = serde_json::to_string(message_id)?;
    let point: serde_json::Value = page
        .evaluate(format!(
            r#"() => {{
                const wanted = {message_id};
                const marker = element => [
                    element.getAttribute('data-message-id'),
                    element.getAttribute('data-id'),
                    element.id
                ];
                const message = Array.from(document.querySelectorAll('[data-message-id], [data-id], [id]'))
                    .find(element => marker(element).includes(wanted));
                if (!message) return null;
                const container = message.closest('[data-tid="chat-pane-item"]') || message.closest('[data-tid="chat-pane-message"]') || message;
                container.scrollIntoView({{ block: 'center', inline: 'nearest' }});
                const rect = container.getBoundingClientRect();
                return {{
                    x: rect.left + rect.width / 2,
                    y: rect.top + rect.height / 2,
                    text: message.getAttribute('aria-label') || message.innerText || message.textContent || ''
                }};
            }}"#,
            message_id = message_id,
        ))
        .await?
        .into_value()?;
    let Some(x) = point.get("x").and_then(serde_json::Value::as_f64) else {
        return Ok(None);
    };
    let Some(y) = point.get("y").and_then(serde_json::Value::as_f64) else {
        return Ok(None);
    };
    let text = point
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .replace('\n', " ")
        .trim()
        .to_string();
    Ok(Some(MessageTarget {
        point: Point::new(x, y),
        text,
    }))
}

async fn click_reply_action(page: &chromiumoxide::Page, message_id: &str) -> Result<bool> {
    let Some(target) = message_center(page, message_id).await? else {
        return Ok(false);
    };
    page.move_mouse(target.point).await?;
    sleep(Duration::from_millis(350)).await;

    let message_id = serde_json::to_string(message_id)?;
    page.evaluate(
        r#"() => document.querySelectorAll('[data-mcp-reply-action]').forEach(element => element.removeAttribute('data-mcp-reply-action'))"#,
    )
    .await?;
    let action: String = page
        .evaluate(format!(
            r#"() => {{
                const wanted = {message_id};
                const marker = element => [
                    element.getAttribute('data-message-id'),
                    element.getAttribute('data-id'),
                    element.id
                ];
                const message = Array.from(document.querySelectorAll('[data-message-id], [data-id], [id]'))
                    .find(element => marker(element).includes(wanted));
                if (!message) return 'none';
                const container = message.closest('[data-tid="chat-pane-item"]') || message.closest('[data-tid="chat-pane-message"]') || message;
                const visible = element => {{
                    const rect = element.getBoundingClientRect();
                    const style = getComputedStyle(element);
                    return style.display !== 'none' && style.visibility !== 'hidden' && rect.width > 0 && rect.height > 0;
                }};
                const label = element => [
                    element.getAttribute('aria-label'),
                    element.getAttribute('title'),
                    element.getAttribute('data-tid'),
                    element.matches('button, [role="button"], [role="menuitem"]') ? element.innerText : ''
                ].filter(Boolean).join(' ').replace(/\s+/g, ' ').trim();
                const controls = root => Array.from(root.querySelectorAll('button, [role="button"], [role="menuitem"], [data-tid*="action" i], [data-tid*="reply" i]'))
                    .filter(visible);
                const reply = controls(container).find(element => /\breply\b/i.test(label(element)));
                if (reply) {{ reply.setAttribute('data-mcp-reply-action', 'reply'); return 'reply'; }}
                const more = controls(container).find(element => /\b(more|options|actions)\b/i.test(label(element)));
                if (more) {{ more.setAttribute('data-mcp-reply-action', 'more'); return 'more'; }}
                return 'none';
            }}"#,
            message_id = message_id,
        ))
        .await?
        .into_value()?;
    if action == "reply" {
        page.find_element(r#"[data-mcp-reply-action="reply"]"#)
            .await?
            .click()
            .await?;
        page.evaluate(
            r#"() => document.querySelectorAll('[data-mcp-reply-action]').forEach(element => element.removeAttribute('data-mcp-reply-action'))"#,
        )
        .await?;
        return Ok(true);
    }
    if action != "more" {
        return Ok(false);
    }

    page.find_element(r#"[data-mcp-reply-action="more"]"#)
        .await?
        .click()
        .await?;

    sleep(Duration::from_millis(250)).await;
    let clicked: bool = page
        .evaluate(
            r#"() => {
                const visible = element => {
                    const rect = element.getBoundingClientRect();
                    const style = getComputedStyle(element);
                    return style.display !== 'none' && style.visibility !== 'hidden' && rect.width > 0 && rect.height > 0;
                };
                const label = element => [
                    element.getAttribute('aria-label'),
                    element.getAttribute('title'),
                    element.getAttribute('data-tid'),
                    element.matches('button, [role="button"], [role="menuitem"]') ? element.innerText : ''
                ].filter(Boolean).join(' ').replace(/\s+/g, ' ').trim();
                const reply = Array.from(document.querySelectorAll('[role="menuitem"], button, [role="button"], [data-tid*="action" i], [data-tid*="reply" i]'))
                    .filter(visible)
                    .find(element => /^reply(?: to message)?$/i.test(label(element)) || /\breply\b/i.test(label(element)));
                if (!reply) return false;
                reply.setAttribute('data-mcp-reply-action', 'menu-reply');
                return true;
            }"#,
        )
        .await?
        .into_value()?;
    if !clicked {
        return Ok(false);
    }
    page.find_element(r#"[data-mcp-reply-action="menu-reply"]"#)
        .await?
        .click()
        .await?;
    page.evaluate(
        r#"() => document.querySelectorAll('[data-mcp-reply-action]').forEach(element => element.removeAttribute('data-mcp-reply-action'))"#,
    )
    .await?;
    Ok(true)
}

pub async fn send_chat_message(
    chat_name: &str,
    message: &str,
    confirm: bool,
    allow_after_reply_failure: bool,
    clear_draft: bool,
) -> Result<SendMessageResult> {
    let chat_name = chat_name.trim();
    if chat_name.is_empty() {
        bail!("chat_name is required");
    }
    validate_outgoing_message(message)?;

    if !confirm {
        return Ok(SendMessageResult {
            chat_name: chat_name.to_string(),
            message: message.to_string(),
            reply_to_message_id: None,
            sent: false,
            confirmation_required: true,
            page_url: teams_url(),
            resolved_mentions: Vec::new(),
            reply_context_verified: None,
            warnings: vec![
                "Preview only: pass confirm=true to open the chat and send this message."
                    .to_string(),
            ],
        });
    }

    reject_unapproved_standalone_fallback(chat_name, message, allow_after_reply_failure)?;

    let session = prepared_page().await?;
    let result = async {
        let navigation_clicked = click_navigation(&session.page, &["Chat", "Chats"]).await?;
        if navigation_clicked {
            sleep(Duration::from_millis(400)).await;
        } else {
            bail!("Could not find the Chat navigation control");
        }

        if !click_visible_label(&session.page, chat_name).await? {
            bail!("Could not find an exact visible chat label {chat_name:?}; refusing to send");
        }
        sleep(Duration::from_millis(600)).await;

        send_from_composer(
            &session.page,
            chat_name,
            message,
            None,
            false,
            None,
            clear_draft,
        )
        .await
    }
    .await;
    finish(session).await?;
    if result.is_ok() {
        clear_failed_reply();
    }
    result
}

pub async fn reply_to_chat_message(
    chat_name: &str,
    message_id: &str,
    message: &str,
    confirm: bool,
    clear_draft: bool,
) -> Result<SendMessageResult> {
    let chat_name = chat_name.trim();
    let message_id = message_id.trim();
    if chat_name.is_empty() {
        bail!("chat_name is required");
    }
    if message_id.is_empty() {
        bail!("message_id is required");
    }
    validate_outgoing_message(message)?;

    if !confirm {
        return Ok(SendMessageResult {
            chat_name: chat_name.to_string(),
            message: message.to_string(),
            reply_to_message_id: Some(message_id.to_string()),
            sent: false,
            confirmation_required: true,
            page_url: teams_url(),
            resolved_mentions: Vec::new(),
            reply_context_verified: None,
            warnings: vec![
                "Preview only: pass confirm=true to open the chat and send this reply.".to_string(),
            ],
        });
    }

    let session = prepared_page().await?;
    let result = async {
        let navigation_clicked = click_navigation(&session.page, &["Chat", "Chats"]).await?;
        if navigation_clicked {
            sleep(Duration::from_millis(400)).await;
        } else {
            bail!("Could not find the Chat navigation control");
        }
        if !click_visible_label(&session.page, chat_name).await? {
            bail!("Could not find an exact visible chat label {chat_name:?}; refusing to reply");
        }
        sleep(Duration::from_millis(600)).await;
        if clear_draft {
            clear_composer(&session.page).await?;
        } else {
            ensure_composer_empty(&session.page).await?;
        }
        let Some(target) = message_center(&session.page, message_id).await? else {
            bail!("Could not find visible Teams message {message_id:?}; refusing to reply");
        };
        if !click_reply_action(&session.page, message_id).await? {
            bail!("Could not find a Reply action for visible Teams message {message_id:?}; refusing to send");
        }
        sleep(Duration::from_millis(300)).await;
        if !wait_for_reply_context(&session.page, message_id, &target.text).await? {
            bail!("Teams did not enter reply mode for visible Teams message {message_id:?}; refusing to send a top-level message");
        }
        send_from_composer(
            &session.page,
            chat_name,
            message,
            Some(message_id.to_string()),
            true,
            Some((message_id, &target.text)),
            clear_draft,
        )
        .await
    }
    .await;
    finish(session).await?;
    if result.is_err() {
        remember_failed_reply(chat_name, message);
    } else {
        clear_failed_reply();
    }
    result
}

pub async fn clear_chat_draft(chat_name: &str, confirm: bool) -> Result<ClearDraftResult> {
    let chat_name = chat_name.trim();
    if chat_name.is_empty() {
        bail!("chat_name is required");
    }
    if !confirm {
        return Ok(ClearDraftResult {
            chat_name: chat_name.to_string(),
            cleared: false,
            confirmation_required: true,
            page_url: teams_url(),
            warnings: vec![
                "Preview only: pass confirm=true to clear the Teams composer draft.".to_string(),
            ],
        });
    }

    let session = prepared_page().await?;
    let result = async {
        let navigation_clicked = click_navigation(&session.page, &["Chat", "Chats"]).await?;
        if navigation_clicked {
            sleep(Duration::from_millis(400)).await;
        } else {
            bail!("Could not find the Chat navigation control");
        }
        if !click_visible_label(&session.page, chat_name).await? {
            bail!(
                "Could not find an exact visible chat label {chat_name:?}; refusing to clear draft"
            );
        }
        sleep(Duration::from_millis(600)).await;
        let cleared = clear_composer(&session.page).await?;
        Ok(ClearDraftResult {
            chat_name: chat_name.to_string(),
            cleared,
            confirmation_required: false,
            page_url: session.page.url().await?.unwrap_or_default(),
            warnings: Vec::new(),
        })
    }
    .await;
    finish(session).await?;
    result
}

pub async fn login() -> Result<()> {
    let profile = profile_dir()?;
    println!("Opening Microsoft Teams in a dedicated browser profile.");
    println!("Profile: {}", profile.display());
    println!(
        "Complete the normal Microsoft login in the browser window; no credentials are read by this CLI."
    );

    let session = open_teams(true).await?;
    let result = timeout(LOGIN_WAIT, async {
        loop {
            let snapshot = page_snapshot(&session.page).await?;
            if snapshot.authenticated {
                return Ok::<PageSnapshot, anyhow::Error>(snapshot);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await;
    let output = match result {
        Ok(Ok(snapshot)) => {
            println!("Teams login detected.");
            print_json(&snapshot)
        }
        Ok(Err(error)) => Err(error),
        Err(_) => bail!("timed out waiting for Teams login; the browser profile was preserved"),
    };
    finish(session).await?;
    output
}

pub async fn status() -> Result<()> {
    print_json(&visible_snapshot().await?)
}

pub async fn inspect() -> Result<()> {
    let session = open_teams(false).await?;
    let result: Result<serde_json::Value> = async {
        let snapshot = wait_for_teams(&session.page).await?;
        let diagnostics: serde_json::Value = session
            .page
            .evaluate(
                r#"() => {
                    const normalize = value => (value || '').replace(/\s+/g, ' ').trim();
                    const truncate = value => normalize(value).slice(0, 300);
                    const elements = [];
                    const seen = new Set();
                    const inspectRoot = (root, rootName, depth) => {
                        if (!root || depth > 3) return;
                        for (const element of Array.from(root.querySelectorAll('*')).slice(0, 5000)) {
                            if (elements.length >= 500) break;
                            const shadow = element.shadowRoot;
                            const role = element.getAttribute('role');
                            const aria = element.getAttribute('aria-label');
                            const dataTid = element.getAttribute('data-tid');
                            const text = truncate(element.innerText || element.textContent);
                            if (role || aria || dataTid || text) {
                                const key = [rootName, element.tagName, role, aria, dataTid, text].join('|');
                                if (!seen.has(key)) {
                                    seen.add(key);
                                    elements.push({
                                        root: rootName,
                                        tag: element.tagName.toLowerCase(),
                                        role,
                                        aria_label: aria,
                                        data_tid: dataTid,
                                        id: element.id || null,
                                        class_name: typeof element.className === 'string' ? element.className.slice(0, 200) : null,
                                        text
                                    });
                                }
                            }
                            if (shadow) inspectRoot(shadow, `${rootName}.shadow`, depth + 1);
                        }
                    };
                    inspectRoot(document, 'document', 0);
                    return {
                        html_chars: document.documentElement?.outerHTML?.length || 0,
                        html_start: (document.documentElement?.outerHTML || '').slice(0, 20000),
                        body_text: truncate(document.body?.innerText),
                        body_text_content: truncate(document.body?.textContent),
                        iframes: Array.from(document.querySelectorAll('iframe')).map(frame => ({
                            src: frame.src || null,
                            title: frame.title || null,
                            name: frame.name || null
                        })),
                        elements
                    };
                }"#,
            )
            .await?
            .into_value()?;
        Ok(serde_json::json!({
            "snapshot": snapshot,
            "dom": diagnostics,
        }))
    }
    .await;
    finish(session).await?;
    print_json(&result?)
}

pub fn logout() -> Result<()> {
    let profile = profile_dir()?;
    if !profile.exists() {
        println!("No local Teams browser profile exists.");
        return Ok(());
    }
    print!(
        "Remove the local Teams browser profile at {}? [y/N] ",
        profile.display()
    );
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if answer.trim().eq_ignore_ascii_case("y") {
        std::fs::remove_dir_all(&profile)?;
        println!("Local Teams browser profile removed.");
    } else {
        println!("Kept the local Teams browser profile.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MentionToken, mention_tokens, resolve_mention_names, validate_outgoing_message};

    #[test]
    fn outgoing_messages_accept_unicode_text() {
        assert!(validate_outgoing_message("når, så, på, å, buffé, også, gå 😀").is_ok());
    }

    #[test]
    fn outgoing_messages_still_reject_multiline_text() {
        assert!(validate_outgoing_message("first line\nsecond line").is_err());
    }

    #[test]
    fn mention_tokens_find_handles_without_matching_email_addresses() {
        assert_eq!(
            mention_tokens("Hi @herman_lindalen_nodeland and mail me@example.com"),
            vec![MentionToken {
                start: 3,
                end: 28,
                query: "herman_lindalen_nodeland".to_string(),
            }]
        );
    }

    #[test]
    fn mention_handles_resolve_to_visible_display_names() {
        let tokens = mention_tokens("@herman_lindalen_nodeland");
        let names = vec!["Herman Lindalen Nodeland".to_string()];
        assert_eq!(
            resolve_mention_names(&tokens, &names).unwrap(),
            vec!["Herman Lindalen Nodeland"]
        );
    }

    #[test]
    fn unresolved_mentions_are_rejected() {
        let tokens = mention_tokens("@nobody");
        let names = vec!["Herman Lindalen Nodeland".to_string()];
        assert!(resolve_mention_names(&tokens, &names).is_err());
    }
}
