use anyhow::{Context, Result};
use chrono::{Local, NaiveDate};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::Value;
use std::{thread, time::Duration as StdDuration};

const UA: &str = "dacon-api-filter/2.0 (+you@example.com)";
const BASE: &str = "https://app.dacon.io/api/v1/competition/list";

// offset은 0부터
const OFFSET_START: u32 = 0;
// 마감 20일 이내
const DEADLINE_DAYS: i64 = 20;

// 키워드
const KEYWORDS: &[&str] = &[
    "ai","인공지능","머신러닝","딥러닝",
    "개발","developer","dev",
    "보안","security",
    "sw","소프트웨어","software",
];

#[derive(Debug, Deserialize, Clone)]
pub struct Item {
    #[serde(default)] cpt_id: i64,
    #[serde(default)] name: String,
    #[serde(default)] name_eng: String,
    #[serde(default)] keyword: String,
    #[serde(default)] keyword_eng: String,
    #[serde(default)] period_start: String, // "YYYY-MM-DD HH:MM:SS"
    #[serde(default)] period_end: String,   // "
}

pub fn collect() -> Result<Vec<Item>> {
    let client = Client::builder().user_agent(UA).build()?;
    let mut offset = OFFSET_START;
    let range = 30u32;

    let mut out: Vec<Item> = Vec::new();

    loop {
        let url = reqwest::Url::parse_with_params(
            BASE,
            &[("offset", offset.to_string()), ("range", range.to_string())],
        )?;

        let resp = client.get(url.clone()).header(ACCEPT, "application/json").send()?;
        let status = resp.status();
        let ctype: String = resp.headers()
            .get(CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned)
            .unwrap_or_default();

        let body = resp.error_for_status()?.text()?;

        if !ctype.to_lowercase().starts_with("application/json") {
            eprintln!("[warn] non-JSON content-type: {ctype}; status={status}");
            eprintln!("snippet: {}", &body.chars().take(200).collect::<String>());
            break;
        }

        let items = parse_items(&body).with_context(|| format!("JSON parse failed at offset={offset}"))?;
        if items.is_empty() { break; }

        // 키워드 + 마감일 20일 이내 필터
        let final_list: Vec<Item> = items
            .into_iter()
            .filter(|it| pass_keyword_filter(it) && within_deadline_days(it, DEADLINE_DAYS))
            .collect();

        out.extend(final_list);

        offset += 1;
        thread::sleep(StdDuration::from_millis(400));
        if offset > OFFSET_START + 10 { break; } // 과도 크롤 방지
    }

    Ok(out)
}

/// 응답이 배열/객체 래퍼 어떤 형태든 Vec<Item>으로 변환
fn parse_items(body: &str) -> Result<Vec<Item>> {
    if let Ok(v) = serde_json::from_str::<Vec<Item>>(body) { return Ok(v); }
    let val: Value = serde_json::from_str(body)?;
    for k in ["list","data","content","items","results"] {
        if let Some(arr) = val.get(k).and_then(|x| x.as_array()) {
            return Ok(from_value_array(arr));
        }
    }
    if let Some(obj) = val.as_object() {
        for (_k, v) in obj {
            if let Some(arr) = v.as_array() {
                if arr.iter().all(|e| e.is_object()) {
                    return Ok(from_value_array(arr));
                }
            }
        }
    }
    let snippet = body.chars().take(200).collect::<String>();
    Err(anyhow::anyhow!("unsupported JSON shape; snippet: {}", snippet))
}

fn from_value_array(arr: &[Value]) -> Vec<Item> {
    arr.iter().filter_map(|e| serde_json::from_value::<Item>(e.clone()).ok()).collect()
}

/// 키워드 필터
fn pass_keyword_filter(it: &Item) -> bool {
    let hay = normalize(&format!("{} {} {} {}", it.name, it.name_eng, it.keyword, it.keyword_eng));
    KEYWORDS.iter().any(|kw| hay.contains(&normalize(kw)))
}

/// 마감일까지 20일 이내면 true
fn within_deadline_days(it: &Item, n: i64) -> bool {
    days_until_deadline(&it.period_end).map(|diff| diff >= 0 && diff <= n).unwrap_or(false)
}

/// D-값(오늘 기준)
fn days_until_deadline(end_str: &str) -> Option<i64> {
    let today = Local::now().date_naive();
    parse_date_ymd(end_str).map(|d| (d - today).num_days())
}

/// "YYYY-MM-DD HH:MM:SS" → NaiveDate
fn parse_date_ymd(s: &str) -> Option<NaiveDate> {
    if s.len() < 10 { return None; }
    NaiveDate::parse_from_str(&s[..10], "%Y-%m-%d").ok()
}

/// 소문자화 + 공백 정규화
fn normalize(s: &str) -> String {
    s.to_lowercase()
        .replace('\u{00A0}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

use crate::notice::{Notice, Source, Kind};

pub fn to_notice_from_dacon(it: &Item) -> Notice {
    // "YYYY-MM-DD HH:MM:SS" -> "YYYY-MM-DD"
    let start = it.period_start.get(0..10).map(|s| s.to_string());
    let end   = it.period_end.get(0..10).map(|s| s.to_string());

    Notice {
        source: Source::Dacon,
        kind: Kind::Contest, // DACON은 공모전 고정
        title: it.name.trim().to_string(),
        url:   format!("https://dacon.io/competitions/official/{}", it.cpt_id),
        start,
        end,
        organizer: None,
        field: None,
    }
}