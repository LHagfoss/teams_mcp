use std::{
    env,
    io::{self, Write},
    path::PathBuf,
    sync::OnceLock,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chromiumoxide::{Browser, browser::BrowserConfig};
use directories::ProjectDirs;
use futures_util::StreamExt;
use serde::Serialize;
use tokio::time::{sleep, timeout};

use crate::data::{ItemsResult, MessagesResult, PageSnapshot, UiItem, VisibleMessage};

const DEFAULT_TEAMS_URL: &str = "https://teams.live.com/v2/";
const LOGIN_WAIT: Duration = Duration::from_secs(10 * 60);
const MAX_VISIBLE_TEXT_CHARS: usize = 20_000;
const MAX_ITEMS: usize = 100;
const MAX_MESSAGES: usize = 100;
const MAX_SCROLL_ATTEMPTS: usize = 12;

static PROFILE_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

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
