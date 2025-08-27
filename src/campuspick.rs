use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate};
use clap::Parser;
use regex::Regex;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use scraper::{Html, Selector};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};
use crate::notice::{Notice, Source, Kind, infer_kind_from_label};

/// 캠퍼스픽 웹 사이트 URL
const WEB_BASE: &str = "https://www.campuspick.com/";

#[derive(Parser, Debug)]
#[command(
    name="campuspick-filter",
    about="Campuspick crawler (contest: category108, activity: title keywords) + detail start~end/company fill"
)]
struct Args {
    /// 대외활동 목록 API
    #[arg(long, default_value = "https://api2.campuspick.com/find/activity/list")]
    activity_api: String,
    /// 공모전 목록 API
    #[arg(long, default_value = "https://api2.campuspick.com/find/activity/list")]
    contest_api: String,

    /// 대외활동 목록 HTTP 메서드
    #[arg(long, default_value = "POST")]
    activity_method: String,
    /// 공모전 목록 HTTP 메서드
    #[arg(long, default_value = "POST")]
    contest_method: String,

    #[arg(long, default_value = "target=2&limit={limit}&offset={offset}")]
    activity_body: String,
    #[arg(long, default_value = "target=1&limit={limit}&offset={offset}&category=108")]
    contest_body: String,

    /// 페이지당 개수
    #[arg(long, default_value_t = 100)]
    limit: usize,
    /// 페이지 수
    #[arg(long, default_value_t = 5)]
    pages: usize,

    /// 마감일까지 남은 일수 필터(20일 이내만)
#[arg(long, default_value_t = 20)]
deadline_days: i64,

    #[arg(long, default_value_t = 300)]
    delay_ms: u64,
}

pub async fn collect() -> Result<Vec<Row>> {
    let args = Args::parse();
    let client = reqwest::Client::builder()
        .user_agent("campuspick-filter/0.6.0 (+contact@example.com)")
        .build()?;

    let mut out = Vec::<Row>::new();

    // 대외활동 수집
    out.extend(
        fetch_one_kind(
            &client, "activity",
            &args.activity_api, &args.activity_method, &args.activity_body,
            args.pages, args.limit, args.deadline_days, args.delay_ms
        ).await?
    );

    // 공모전 수집
    out.extend(
        fetch_one_kind(
            &client, "contest",
            &args.contest_api, &args.contest_method, &args.contest_body,
            args.pages, args.limit, args.deadline_days, args.delay_ms
        ).await?
    );

    out.sort_by(|a,b| a.start.is_none().cmp(&b.start.is_none())
        .then(a.start.cmp(&b.start))
        .then(a.end.cmp(&b.end))
        .then(a.title.cmp(&b.title)));
    Ok(out)
}

#[derive(Clone, Debug)]
pub struct Row {
    pub kind: String,          // activity / contest
    pub title: String,         // 제목
    pub url: String,           // 상세 URL
    pub start: Option<String>, // 시작일(YYYY-MM-DD)
    pub end: Option<String>,   // 마감일(YYYY-MM-DD)
    pub company: Option<String>, // 주최/주관(가능하면 여러 값을 " / "로 결합)
}

async fn fetch_one_kind(
    client: &reqwest::Client,
    kind: &str,
    api: &str, method: &str, body_tpl: &str,
    pages: usize, limit: usize, deadline_days: i64,
    delay_ms: u64,
) -> Result<Vec<Row>> {
    let mut out = Vec::<Row>::new();
    let mut seen = HashSet::<(String, String)>::new(); // (kind, id) 중복방지

    for page in 1..=pages {
        let offset = (page - 1) * limit;
        let body = body_tpl.replace("{limit}", &limit.to_string())
                           .replace("{offset}", &offset.to_string());

        let mut req = if method.eq_ignore_ascii_case("POST") {
            client.post(api)
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(body)
        } else {
            let url = if api.contains('?') { format!("{api}&{body}") } else { format!("{api}?{body}") };
            client.get(url)
        };
        req = req.header(ACCEPT, "application/json, text/plain, */*")
                 .header("Origin", WEB_BASE)
                 .header("Referer", format!("{WEB_BASE}{kind}"));

        let resp   = req.send().await?;
        let status = resp.status();
        let headers = resp.headers().clone(); 
        let text   = resp.text().await?;
        let ctype  = headers.get(CONTENT_TYPE).and_then(|h| h.to_str().ok()).unwrap_or("");

        if !status.is_success() || !ctype.starts_with("application/json") { break; }

        let v: Value = serde_json::from_str(&text).with_context(|| "invalid JSON")?;
        let Some(arr) = find_array(&v) else { break; };

        'each: for it in arr {
            // 식별자 확보
            let Some(id) = get_id(it) else { continue };
            if !seen.insert((kind.to_string(), id.clone())) { continue; }

            // 제목 확보
            let title = first_text(it, &["title","name","subject"]).unwrap_or_default();
            if title.is_empty() { continue; }

            // 종류(대외활동 or 공모전)별 1차 필터
            if kind == "contest" && !match_category_108(it) { continue 'each; }
            if kind == "activity" && !title_keyword_hit(&title) { continue 'each; }

            // 목록 JSON에서 날짜/주최 추정
            let start0 = it.get("startDate").and_then(|x| x.as_str()).map(normalize_date);
            let end0   = it.get("endDate").and_then(|x| x.as_str()).map(normalize_date)
                        .or_else(|| it.get("deadline").and_then(|x| x.as_str()).map(normalize_date));
            let company0 = first_company(it);

            // 상세에서 startDate/endDate/company 보완 수집
            let (start1, end1, company1) = fill_detail_fields(client, kind, &id, end0.as_deref()).await;

            let start = start0.or(start1);
            let end   = end0.or(end1);
            let company = company0.or(company1);
            // D-day 필터링: 기본 20일(오늘~20일)만, 지난 마감들 제외
            let Some(ref e) = end else { continue 'each; };
            let days = days_until(e);
            if !(0..=deadline_days).contains(&days) { continue 'each; }

            out.push(Row {
                kind: kind.to_string(),
                title,
                url: build_detail_url(kind, &id),
                start, end, company,
            });
        }
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
    Ok(out)
}

fn find_array<'a>(v: &'a Value) -> Option<&'a Vec<Value>> {
    if let Some(a) = v.as_array() { return Some(a); }
    for k in ["items","list","data","results","content","rows","posts","payload"] {
        if let Some(a) = v.get(k).and_then(|x| x.as_array()) { return Some(a); }
    }
    if let Some(obj) = v.as_object() {
        for vv in obj.values() {
            if let Some(a) = find_array(vv) { return Some(a); }
        }
    }
    None
}

fn first_text(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            let t = s.trim();
            if !t.is_empty() { return Some(t.to_string()); }
        }
    }
    None
}

/// ID 문자열을 얻음
fn get_id(v: &Value) -> Option<String> {
    for k in ["id","idx","activityId","contestId","postId","aid","cid"] {
        if let Some(x) = v.get(k) {
            if let Some(s) = x.as_str() { if !s.is_empty() { return Some(s.to_string()); } }
            if let Some(n) = x.as_i64() { return Some(n.to_string()); }
            if let Some(n) = x.as_u64() { return Some(n.to_string()); }
        }
    }
    None
}

/// 상세 페이지 URL 구성
fn build_detail_url(kind: &str, id: &str) -> String {
    match kind { "activity" => format!("{WEB_BASE}activity/view?id={id}"),
                 _          => format!("{WEB_BASE}contest/view?id={id}") }
}

/// 카테고리 필드가 108(IT/소프트웨어/게임)인지 판별
fn match_category_108(v: &Value) -> bool {
    for k in ["category","categoryId","category_idx","categoryId1","category1","categories"] {
        if let Some(x) = v.get(k) {
            if let Some(n) = x.as_i64() { if n == 108 { return true; } }
            if let Some(s) = x.as_str() {
                if s.trim() == "108" { return true; }
                if s.split(|c:char| c.is_ascii_punctuation() || c.is_whitespace()).any(|t| t=="108") { return true; }
            }
            if let Some(a) = x.as_array() {
                if a.iter().any(|e| e.as_i64()==Some(108) || e.as_str()==Some("108")) { return true; }
            }
        }
    }
    false
}

/// 활동 제목에 키워드가 포함 검사
fn title_keyword_hit(title: &str) -> bool {
    const KWS: &[&str] = &[
        "IT","SW","코딩","소프트웨어","컴퓨터","보안","정보보호","KISIA","개인정보","개발자","AI","엔지니어","부트캠프"
    ];
    let t = normalize(title);
    KWS.iter().any(|kw| t.contains(&normalize(kw)))
}

/// 날짜 문자열을 YYYY-MM-DD로 통일
fn normalize_date(s: &str) -> String {
    let mut t = s.trim().to_string();
    t = t.replace('.', "-").replace('/', "-");
    if t.len() >= 10 { t[..10].to_string() } else { t }
}

/// today→마감일까지 남은 일수 계산
fn days_until(end_ymd: &str) -> i64 {
    let today = Local::now().date_naive();
    NaiveDate::parse_from_str(end_ymd, "%Y-%m-%d")
        .map(|d| (d - today).num_days())
        .unwrap_or(i64::MAX)
}

fn normalize(s: &str) -> String {
    s.to_lowercase()
        .replace('\u{00A0}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn first_company(v: &Value) -> Option<String> {
    let keys = [
        "company","company_name","company1","company2","company3",
        "org","organization","host","hostName","organizer","sponsor","hostOrg","host_org"
    ];
    for k in keys {
        if let Some(val) = v.get(k) {
            if let Some(s) = val.as_str() {
                let s = s.trim();
                if !s.is_empty() { return Some(s.to_string()); }
            }
            if let Some(arr) = val.as_array() {
                let joined = arr.iter().filter_map(|e| e.as_str()).map(|s| s.trim())
                    .filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" / ");
                if !joined.is_empty() { return Some(joined); }
            }
        }
    }
    None
}

fn extract_company_from_text(text: &str) -> Option<String> {
    let re = Regex::new(r"(주최|주관)\s*[:：]?\s*([^\n]+)").ok()?;
    let cap = re.captures(text)?;
    let raw = cap.get(2)?.as_str().trim();
    let parts = raw.split(|c: char| c == '/' || c == '|' || c == '·' || c == ',' )
        .map(|s| s.trim()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
    if parts.is_empty() { None } else { Some(parts.join(" / ")) }
}

async fn fill_detail_fields(
    client: &reqwest::Client,
    kind: &str,
    id: &str,
    end_hint: Option<&str>,
) -> (Option<String>, Option<String>, Option<String>) {
    let page_url = build_detail_url(kind, id);
    if let Ok(resp) = client.get(&page_url).send().await {
        if resp.status().is_success() {
            if let Ok(html) = resp.text().await {
                let doc = Html::parse_document(&html);
                let script_sel = Selector::parse("script").unwrap();
                let mut scripts_text = String::new();
                for s in doc.select(&script_sel) {
                    scripts_text.push_str(&s.text().collect::<String>());
                    scripts_text.push('\n');
                }

                let re_sd = Regex::new(r#"startDate\s*:\s*\"([0-9]{4}[-./][0-9]{2}[-./][0-9]{2})\""#).unwrap();
                let re_ed = Regex::new(r#"endDate\s*:\s*\"([0-9]{4}[-./][0-9]{2}[-./][0-9]{2})\""#).unwrap();
                let re_company = Regex::new(r#"company\d*\s*:\s*\"([^\"]+)\""#).unwrap();

                let s = re_sd.captures(&scripts_text).map(|c| normalize_date(c.get(1).unwrap().as_str()));
                let e = re_ed.captures(&scripts_text).map(|c| normalize_date(c.get(1).unwrap().as_str()));

                let mut companies: Vec<String> = Vec::new();
                for cap in re_company.captures_iter(&scripts_text) {
                    let v = cap.get(1).unwrap().as_str().trim();
                    if !v.is_empty() { companies.push(v.to_string()); }
                }
                companies.sort();
                companies.dedup();
                let company_inline = if companies.is_empty() { None } else { Some(companies.join(" / ")) };

                if s.is_some() || e.is_some() || company_inline.is_some() {
                    return (s, e, company_inline);
                }

                let text = extract_relevant_text(&doc);
                if let Some((s2,e2)) = parse_dates_from_korean_or_numeric(&text, end_hint) {
                    let comp = extract_company_from_text(&text);
                    return (s2, e2, comp);
                }
            }
        }
    }

    let json_candidates = [
        format!("https://api2.campuspick.com/find/{kind}/view?id={id}"),
        format!("https://api2.campuspick.com/{kind}/view?id={id}"),
        format!("https://api2.campuspick.com/find/{kind}/detail?id={id}"),
        format!("https://api2.campuspick.com/{kind}/detail?id={id}"),
    ];
    for url in json_candidates {
        if let Ok(resp) = client.get(&url).header(ACCEPT, "application/json").send().await {
            let status = resp.status();
            let headers = resp.headers().clone();
            let txt = resp.text().await.unwrap_or_default();
            let is_json = headers.get(CONTENT_TYPE).and_then(|h| h.to_str().ok())
                .map(|s| s.starts_with("application/json")).unwrap_or(false);
            if !status.is_success() || !is_json { continue; }

            if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                let s = v.get("startDate").and_then(|x| x.as_str())
                         .or_else(|| v.pointer("/data/startDate").and_then(|x| x.as_str()))
                         .map(normalize_date);
                let e = v.get("endDate").and_then(|x| x.as_str())
                         .or_else(|| v.get("deadline").and_then(|x| x.as_str()))
                         .or_else(|| v.pointer("/data/endDate").and_then(|x| x.as_str()))
                         .map(normalize_date);

                let company = first_company(&v)
                    .or_else(|| v.pointer("/data").and_then(first_company));

                if s.is_some() || e.is_some() || company.is_some() {
                    return (s, e, company);
                }
            }
        }
    }

    if let Ok(resp) = client.get(&build_detail_url(kind, id)).send().await {
        if resp.status().is_success() {
            if let Ok(html) = resp.text().await {
                let doc = Html::parse_document(&html);
                let text = extract_relevant_text(&doc);
                let de = parse_dates_from_korean_or_numeric(&text, end_hint);
                let comp = extract_company_from_text(&text);
                if de.is_some() || comp.is_some() {
                    let (s,e) = de.unwrap_or((None, None));
                    return (s, e, comp);
                }
            }
        }
    }

    (None, None, None)
}

fn extract_relevant_text(doc: &Html) -> String {
    let sec_sel = Selector::parse("#container .section, .section").unwrap();
    let p_sel   = Selector::parse("p, li, dd, div").unwrap();
    let mut candidate_text = String::new();
    for sec in doc.select(&sec_sel) {
        let sec_txt = sec.text().collect::<String>();
        if ["접수 기간","모집 기간","활동 기간","교육 기간","신청 기간","운영 기간"]
            .iter().any(|kw| sec_txt.contains(kw))
        {
            for node in sec.select(&p_sel) {
                candidate_text.push_str(&node.text().collect::<String>());
                candidate_text.push('\n');
            }
        }
    }
    if candidate_text.trim().is_empty() {
        candidate_text = doc.root_element().text().collect::<String>();
    }
    normalize_whitespace(&candidate_text)
}

/// 한국어/숫자 범위 표기에서 날짜(시작/종료)를 파싱
fn parse_dates_from_korean_or_numeric(text: &str, end_hint: Option<&str>) -> Option<(Option<String>, Option<String>)> {
    // 숫자 yyyy-mm-dd ~ yyyy-mm-dd
    let re_num = Regex::new(
        r"(20\d{2}[-./]\d{1,2}[-./]\d{1,2})\s*[~\-–]\s*(20\d{2}[-./]\d{1,2}[-./]\d{1,2})"
    ).unwrap();
    if let Some(caps) = re_num.captures(text) {
        let s = normalize_date(caps.get(1).unwrap().as_str());
        let e = normalize_date(caps.get(2).unwrap().as_str());
        return Some((Some(s), Some(e)));
    }

    // 한국어 "(연) m월 d일 ~ (연) m월 d일"
    let re_kr = Regex::new(
        r"(?:(?P<y1>20\d{2})\s*년\s*)?(?P<m1>\d{1,2})\s*월\s*(?P<d1>\d{1,2})\s*일(?:\([^)]*\))?\s*[~\-–]\s*(?:(?P<y2>20\d{2})\s*년\s*)?(?P<m2>\d{1,2})\s*월\s*(?P<d2>\d{1,2})\s*일"
    ).unwrap();
    if let Some(caps) = re_kr.captures(text) {
        let y2 = caps.name("y2").and_then(|m| m.as_str().parse::<i32>().ok())
            .or_else(|| end_hint.and_then(|e| e.get(0..4)).and_then(|y| y.parse().ok()))
            .unwrap_or_else(|| Local::now().year());
        let m2: u32 = caps.name("m2").unwrap().as_str().parse().unwrap_or(1);
        let d2: u32 = caps.name("d2").unwrap().as_str().parse().unwrap_or(1);

        let mut y1 = caps.name("y1").and_then(|m| m.as_str().parse::<i32>().ok()).unwrap_or(y2);
        let m1: u32 = caps.name("m1").unwrap().as_str().parse().unwrap_or(1);
        let d1: u32 = caps.name("d1").unwrap().as_str().parse().unwrap_or(1);

        if caps.name("y1").is_none() && caps.name("y2").is_none() && m1 > m2 { y1 = y2 - 1; }

        let s = format!("{:04}-{:02}-{:02}", y1, m1, d1);
        let e = format!("{:04}-{:02}-{:02}", y2, m2, d2);
        return Some((Some(s), Some(e)));
    }

    let re_single = Regex::new(
        r"(?:(?P<y>20\d{2})\s*년\s*)?(?P<m>\d{1,2})\s*월\s*(?P<d>\d{1,2})\s*일\s*(?:마감|까지|접수마감)?"
    ).unwrap();
    if let Some(caps) = re_single.captures(text) {
        let y = caps.name("y").and_then(|m| m.as_str().parse::<i32>().ok())
            .or_else(|| end_hint.and_then(|e| e.get(0..4)).and_then(|y| y.parse().ok()))
            .unwrap_or_else(|| Local::now().year());
        let m: u32 = caps.name("m").unwrap().as_str().parse().unwrap_or(1);
        let d: u32 = caps.name("d").unwrap().as_str().parse().unwrap_or(1);
        let e = format!("{:04}-{:02}-{:02}", y, m, d);
        return Some((None, Some(e)));
    }

    None
}

fn normalize_whitespace(s: &str) -> String {
    let mut t = s.replace('\u{00A0}', " ");
    t = Regex::new(r"\s+").unwrap().replace_all(&t, " ").into_owned();
    t.trim().to_string()
}

// === Notice 어댑터 ===
pub fn to_notice_from_campuspick(r: &Row) -> Notice {
    let mut kind = infer_kind_from_label(&r.kind, Kind::Contest);

    let url_lc = r.url.to_lowercase();
    if url_lc.contains("/activity/") || url_lc.contains("activity/view") {
        kind = Kind::Activity;
    } else if url_lc.contains("/contest/") || url_lc.contains("contest/view") {
        kind = Kind::Contest;
    }

    Notice {
        source: Source::Campuspick,
        kind,
        title: r.title.clone(),
        url:   r.url.clone(),
        start: r.start.clone(),
        end:   r.end.clone(),
        organizer: r.company.clone(),
        field: None,
    }
}