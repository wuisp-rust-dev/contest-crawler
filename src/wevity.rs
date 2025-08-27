// src/wevity.rs
use anyhow::Result;
use chrono::{NaiveDate, Local};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT, ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, PRAGMA, REFERER};
use reqwest::redirect::Policy;
use scraper::{Html, Selector, ElementRef};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio::{task::JoinSet, time::{sleep, timeout}};
use url::Url;

#[derive(Debug, Clone)]
pub struct Contest {
    pub title: String,
    pub organizer: String,
    pub url: String,
    pub start: Option<String>,
    pub end: Option<String>,
    pub category: String,      // "공모전" or "대외활동"
    pub field: Option<String>, // 리스트의 "div.sub-tit" 원문
}

/* ================= HTTP 공통 ================= */

fn build_client() -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36"
    ));
    headers.insert(ACCEPT, HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"));
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("ko-KR,ko;q=0.9,en-US;q=0.8,en;q=0.7"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(PRAGMA, HeaderValue::from_static("no-cache"));

    Ok(reqwest::Client::builder()
        .pool_max_idle_per_host(2)
        .tcp_keepalive(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(3)) // 개별 요청 상한(추가로 아래 timeout()으로 더 타이트하게 감쌈)
        .redirect(Policy::limited(10))
        .default_headers(headers)
        .build()?)
}

async fn prewarm_home(client: &reqwest::Client) {
    let _ = timeout(Duration::from_secs(2), client.get("https://www.wevity.com/").send()).await;
}

fn looks_like_bot(status: reqwest::StatusCode, body: &str) -> bool {
    status.as_u16() == 403
        || status.as_u16() == 503
        || body.contains("cf-ray")
        || body.contains("Attention Required")
        || body.contains("Please wait while your request is being verified")
}

async fn fetch_html_with_retry(client: &reqwest::Client, url: &str, referer: &str) -> Option<String> {
    let mut backoff = 300u64;
    for _ in 0..3 {
        let fut = client.get(url).header(REFERER, referer).send();
        match timeout(Duration::from_millis(2200), fut).await {
            Ok(Ok(resp)) => {
                let status = resp.status();
                let text = match resp.text().await { Ok(t) => t, Err(_) => String::new() };
                if status.is_success() && !looks_like_bot(status, &text) && !text.is_empty() {
                    return Some(text);
                }
            }
            _ => {}
        }
        sleep(Duration::from_millis(backoff)).await;
        backoff = (backoff * 2).min(1500);
    }
    None
}

/* ================= 상세 파싱 ================= */

async fn fetch_detail_and_build_contest(
    client: reqwest::Client,
    url_abs: String,
    title: String,
    field_text: Option<String>,
    category_label: &str,
    list_referer: &str,
) -> Option<Contest> {
    let html = fetch_html_with_retry(&client, &url_abs, list_referer).await?;
    let doc = Html::parse_document(&html);

    // 기간
    let sel_during = Selector::parse(r#"input[name="during"]"#).ok()?;
    let raw = doc.select(&sel_during).next()
        .and_then(|n| n.value().attr("value")).unwrap_or("");
    let (apply_start, apply_end) = parse_period_value(raw);

    // 주최/주관
    let mut organizer = String::new();
    let sel_li  = Selector::parse("ul.cd-info-list > li").ok()?;
    let sel_tit = Selector::parse("span.tit").ok()?;
    for li in doc.select(&sel_li) {
        let label = li.select(&sel_tit).next()
            .map(|n| norm_text(&n.text().collect::<String>())).unwrap_or_default();
        if label.contains("주최") || label.contains("주관") {
            let full = norm_text(&li.text().collect::<String>());
            let mut value = full.replacen(&label, "", 1);
            value = value.trim_start().to_string();
            organizer = norm_text(&value);
            break;
        }
    }

    Some(Contest {
        title,
        organizer,
        url: url_abs,
        start: apply_start,
        end: apply_end,
        category: category_label.to_string(),
        field: field_text,
    })
}

/* ================= 카테고리 크롤러(시간예산 보장) ================= */

async fn scrape_wevity_category(base_url: &str, category_label: &str) -> Result<Vec<Contest>> {
    let client = build_client()?;
    prewarm_home(&client).await;

    // ===== 시간/페이지/동시성 파라미터 =====
    let budget_secs: u64 = std::env::var("WEVITY_BUDGET_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(9);
    let max_pages: usize = std::env::var("WEVITY_MAX_PAGES").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    let max_conc: usize  = std::env::var("WEVITY_MAX_CONC").ok().and_then(|s| s.parse().ok()).unwrap_or(4);

    let started = Instant::now();
    let budget  = Duration::from_secs(budget_secs);

    let sel_tit_link = Selector::parse("div.hide-tit > a, div.tit > a").unwrap();
    let sel_subtit   = Selector::parse("div.sub-tit").unwrap();

    let mut items: Vec<Contest> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    'page_loop: for page in 1..=max_pages {
        if started.elapsed() >= budget { break; }

        let url = format!("{}&gp={}", base_url, page);
        let html = match fetch_html_with_retry(&client, &url, base_url).await {
            Some(h) => h,
            None => { sleep(Duration::from_millis(200)).await; continue; }
        };
        let doc = Html::parse_document(&html);

        // 리스트에서 후보 수집
        let mut entries: Vec<(String, String, Option<String>)> = Vec::new();
        for a in doc.select(&sel_tit_link) {
            let title = norm_text(&a.text().collect::<String>());
            let href  = a.value().attr("href").unwrap_or("").trim();
            if title.is_empty() || href.is_empty() { continue; }
            let url_abs = match Url::parse("https://www.wevity.com").and_then(|u| u.join(href)) {
                Ok(u) => u.to_string(),
                Err(_) => continue,
            };

            let mut field_text: Option<String> = None;
            if let Some(li) = find_ancestor_li(&a) {
                if let Some(sub) = li.select(&sel_subtit).next() {
                    field_text = Some(norm_text(&sub.text().collect::<String>()));
                }
            }

            if !seen.insert(url_abs.clone()) { continue; }
            entries.push((title, url_abs, field_text));
        }

        // 상세 병렬 (시간예산 체크)
        let mut join = JoinSet::new();
        let mut i = 0usize;
        let total = entries.len();
        let mut got = 0usize;

        while i < total {
            // 슬롯 채우기
            while join.len() < max_conc && i < total {
                if started.elapsed() >= budget { break 'page_loop; }
                let (title, url_abs, field_text) = entries[i].clone();
                i += 1;

                let client_cl = client.clone();
                let cat = category_label.to_owned();
                let referer = url.clone();
                join.spawn(async move {
                    fetch_detail_and_build_contest(client_cl, url_abs, title, field_text, &cat, &referer).await
                });
            }

            if started.elapsed() >= budget { break 'page_loop; }

            if let Some(res) = join.join_next().await {
                if let Ok(Some(contest)) = res {
                    items.push(contest);
                    got += 1;
                }
            } else { break; }
        }

        // 남은 작업 수거
        while let Some(res) = join.join_next().await {
            if started.elapsed() >= budget { break 'page_loop; }
            if let Ok(Some(contest)) = res {
                items.push(contest);
                got += 1;
            }
        }

        // 페이지 이동 간 살짝 쉼
        sleep(Duration::from_millis(150)).await;

        // 이 페이지에서 아무 것도 못 얻었으면 다음으로
        if got == 0 && started.elapsed() >= budget {
            break;
        }
    }

    // 오늘 이후만 남기기
    let today = Local::now().date_naive();
    items.retain(|c| {
        if let Some(ref end_str) = c.end {
            if let Ok(end_date) = NaiveDate::parse_from_str(end_str, "%Y-%m-%d") {
                if end_date < today { return false; }
            }
        }
        true
    });

    // === 마감이 20일 이내인 것만 남기기 ===
    let cutoff = today
        .checked_add_signed(chrono::Duration::days(20))
        .unwrap();

    items.retain(|c| {
        if let Some(ref end_str) = c.end {
            if let Ok(end_date) = NaiveDate::parse_from_str(end_str, "%Y-%m-%d") {
            // 오늘 포함 ~ 20일 이내만 남김
            return end_date <= cutoff;
            }
        }
        false // end가 없는 경우는 제외
    });

    Ok(items)
}

// 활동 제목 키워드(전부 소문자)
const ACTIVITY_KEYWORDS: &[&str] = &[
    "it","sw","코딩","소프트웨어","컴퓨터","보안","정보보호","kisia",
    "개인정보","개발자","ai","엔지니어","부트캠프",
];

fn matches_activity_keywords(title: &str) -> bool {
    let t = title.to_lowercase(); // 한글은 소문자 영향 없음, 영문만 통일
    ACTIVITY_KEYWORDS.iter().any(|k| t.contains(*k))
}

/* ================= 외부 공개 함수 ================= */

pub async fn scrape_wevity_contests() -> Result<Vec<Contest>> {
    let urls = [
        "https://www.wevity.com/?c=find&s=1&gub=1&cidx=20",
        "https://www.wevity.com/?c=find&s=1&gub=1&cidx=21",
    ];
    let mut all = Vec::new();
    let mut seen = HashSet::new();
    for u in urls {
        let mut batch = scrape_wevity_category(u, "공모전").await?;
        batch.retain(|c| seen.insert(c.url.clone()));
        all.extend(batch);
    }
    Ok(all)
}

pub async fn scrape_wevity_activities() -> Result<Vec<Contest>> {
    let mut items = scrape_wevity_category("https://www.wevity.com/?c=active&s=1", "대외활동").await?;

    // 제목 필터링
    items.retain(|c| matches_activity_keywords(&c.title));

    Ok(items)
}

/* ================= 유틸 ================= */

fn norm_text(s: &str) -> String {
    let t = s.replace('\u{00A0}', " ")
        .replace('\r', " ")
        .replace('\n', " ")
        .replace('\t', " ");
    t.split_whitespace().collect::<Vec<_>>().join(" ").trim().to_string()
}

fn parse_period_value(v: &str) -> (Option<String>, Option<String>) {
    let parts: Vec<&str> = v.split('~').collect();
    let start = parts.get(0).and_then(|s| parse_ymd_str(s));
    let end   = parts.get(1).and_then(|s| parse_ymd_str(s));
    (start, end)
}

fn parse_ymd_str(s: &str) -> Option<String> {
    let keep: String = s.chars().filter(|&c| c.is_ascii_digit() || c == '-').collect();
    if keep.len() < 10 { return None; }
    let ymd = &keep[..10];
    NaiveDate::parse_from_str(ymd, "%Y-%m-%d").ok()?;
    Some(ymd.to_string())
}

fn find_ancestor_li<'a>(a: &ElementRef<'a>) -> Option<ElementRef<'a>> {
    for node in a.ancestors() {
        if let Some(el) = ElementRef::wrap(node) {
            if el.value().name() == "li" { return Some(el); }
        }
    }
    None
}

// === Notice 어댑터 ===
use crate::notice::{Notice, Source, Kind};
pub fn to_notice_from_wevity(c: &Contest) -> Notice {
    Notice {
        source: Source::Wevity,
        kind: if c.category == "대외활동" { Kind::Activity } else { Kind::Contest },
        title: c.title.clone(),
        url: c.url.clone(),
        start: c.start.clone(),
        end: c.end.clone(),
        organizer: if c.organizer.trim().is_empty() { None } else { Some(c.organizer.clone()) },
        field: c.field.clone(),
    }
}



