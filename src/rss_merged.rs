// src/rss_merged.rs
use anyhow::Result;
use chrono::NaiveDate;
use std::collections::HashSet;

use crate::notice::{Notice, Kind};
use crate::rss_write::write_rss_feed;

/// 여러 소스에서 받은 Vec<Notice>들을 합쳐서
/// - URL 기준 중복 제거
/// - 날짜 최신순 정렬
/// - tie-breaker: kind → title
pub fn merge_notices(sources: Vec<Vec<Notice>>) -> Vec<Notice> {
    // 1) 평탄화
    let mut all: Vec<Notice> = sources.into_iter().flatten().collect();

    // 2) URL 기준 dedup (URL이 비어있으면 title+kind+source)
    let mut seen = HashSet::new();
    all.retain(|n| {
        let key = if n.url.is_empty() {
            format!("{}-{:?}-{:?}", n.title, n.source, n.kind)
        } else {
            n.url.clone()
        };
        seen.insert(key)
    });

    // 3) 정렬: start→end 최신순, 같으면 Kind→title
    all.sort_by(|a, b| {
        let ka = date_key(a);
        let kb = date_key(b);
        match kb.cmp(&ka) {
            std::cmp::Ordering::Equal => {
                // Contest 먼저, Activity 나중
                kind_rank(&a.kind).cmp(&kind_rank(&b.kind)).then(a.title.cmp(&b.title))
            }
            other => other,
        }
    });

    all
}

/// 정렬용 날짜 키: start(우선) → end → None
fn date_key(n: &Notice) -> Option<NaiveDate> {
    n.start
        .as_deref()
        .and_then(parse_ymd)
        .or_else(|| n.end.as_deref().and_then(parse_ymd))
}

fn parse_ymd(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Kind 우선순위: Contest(0) → Activity(1)
fn kind_rank(k: &Kind) -> u8 {
    match k {
        Kind::Contest  => 0,
        Kind::Activity => 1,
    }
}

/// 합치고 바로 RSS 파일로 저장하는 헬퍼
pub fn write_merged_rss(
    sources: Vec<Vec<Notice>>,
    channel_title: &str,
    channel_link: &str,
    channel_desc: &str,
    output_file: &str,
) -> Result<()> {
    let merged = merge_notices(sources);
    write_rss_feed(&merged, channel_title, channel_link, channel_desc, output_file)
}