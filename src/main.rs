// src/main.rs
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::time::Duration;

mod notice;
mod wevity;
mod campuspick;
mod dacon;

mod rss_write;
mod rss_merged;

use notice::Notice;
use tokio::time::timeout;

#[tokio::main]
async fn main() -> Result<()> {
    eprintln!("[start] main");

    // ── ENV로 조절 가능한 타임아웃/프리뷰/경로
    let to_wevity: u64     = std::env::var("TO_WEVITY").ok().and_then(|s| s.parse().ok()).unwrap_or(25);
    let to_campuspick: u64 = std::env::var("TO_CAMPUS").ok().and_then(|s| s.parse().ok()).unwrap_or(25);
    let to_dacon: u64      = std::env::var("TO_DACON").ok().and_then(|s| s.parse().ok()).unwrap_or(25);
    let preview_n: usize   = std::env::var("PREVIEW_N").ok().and_then(|s| s.parse().ok()).unwrap_or(30);

    // RSS 출력 경로(없으면 etc-rss 밑으로)
    let out_dir = std::env::var("RSS_DIR").unwrap_or_else(|_| "etc-rss".into());
    let p_wevity   = std::env::var("RSS_WEVITY").unwrap_or_else(|_| format!("{out_dir}/wevity_rss.xml"));
    let p_campus   = std::env::var("RSS_CAMPUS").unwrap_or_else(|_| format!("{out_dir}/campus_pick_rss.xml"));
    let p_dacon    = std::env::var("RSS_DACON").unwrap_or_else(|_| format!("{out_dir}/dacon_rss.xml"));
    let p_merged   = std::env::var("RSS_MERGED").unwrap_or_else(|_| format!("{out_dir}/merged_rss.xml"));

    // ── 1) wevity: 공모전/대외활동 동시에 + 개별 타임아웃
    let wevity_fut = async {
        eprintln!("[wevity] fetching…");
        let (c_res, a_res) = tokio::join!(
            timeout(Duration::from_secs(to_wevity), wevity::scrape_wevity_contests()),
            timeout(Duration::from_secs(to_wevity), wevity::scrape_wevity_activities()),
        );
        let contests   = c_res.context("wevity contests timeout")??;
        let activities = a_res.context("wevity activities timeout")??;

        let mut out: Vec<Notice> = Vec::with_capacity(contests.len() + activities.len());
        out.extend(contests.iter().map(wevity::to_notice_from_wevity));
        out.extend(activities.iter().map(wevity::to_notice_from_wevity));
        Ok::<Vec<Notice>, anyhow::Error>(out)
    };

    // ── 2) campuspick: async → timeout
    let campuspick_fut = async {
        eprintln!("[campuspick] fetching…");
        let rows = timeout(Duration::from_secs(to_campuspick), campuspick::collect())
            .await
            .context("campuspick timeout")??;
        let notices = rows
            .iter()
            .map(campuspick::to_notice_from_campuspick)
            .collect::<Vec<_>>();
        Ok::<Vec<Notice>, anyhow::Error>(notices)
    };

    // ── 3) dacon: blocking → spawn_blocking + timeout
    let dacon_fut = async {
        use tokio::task::JoinHandle;
        eprintln!("[dacon] fetching…");

        let join: JoinHandle<anyhow::Result<Vec<dacon::Item>>> =
            tokio::task::spawn_blocking(dacon::collect);

        let join_out = timeout(Duration::from_secs(to_dacon), join)
            .await
            .map_err(|_| anyhow::anyhow!("dacon timeout"))?;

        let rows: Vec<dacon::Item> = match join_out {
            Ok(Ok(v))  => v,
            Ok(Err(e)) => return Err(e),
            Err(e)     => return Err(anyhow::Error::new(e)),
        };

        let notices = rows
            .iter()
            .map(dacon::to_notice_from_dacon)
            .collect::<Vec<_>>();
        Ok::<Vec<Notice>, anyhow::Error>(notices)
    };

    // ── 4) 병렬 수집(부분 성공 허용)
    let (wevity_v, campuspick_v, dacon_v) = tokio::join!(wevity_fut, campuspick_fut, dacon_fut);

    let wevity_v     = wevity_v.unwrap_or_else(|e| { eprintln!("[wevity] skipped: {e:#}"); Vec::new() });
    let campuspick_v = campuspick_v.unwrap_or_else(|e| { eprintln!("[campuspick] skipped: {e:#}"); Vec::new() });
    let dacon_v      = dacon_v.unwrap_or_else(|e| { eprintln!("[dacon] skipped: {e:#}"); Vec::new() });

    // ── 5) (옵션) 개별 RSS 파일 생성
    std::fs::create_dir_all(&out_dir).ok();

    if !wevity_v.is_empty() {
        if let Err(e) = rss_write::write_rss_feed(
            &wevity_v,
            "Wevity RSS",
            "https://www.wevity.com",
            "위비티 공모전/대외활동",
            &p_wevity,
        ) {
            eprintln!("[rss_write] wevity failed: {e:?}");
        }
    }
    if !campuspick_v.is_empty() {
        if let Err(e) = rss_write::write_rss_feed(
            &campuspick_v,
            "Campuspick RSS",
            "https://www.campuspick.com",
            "캠퍼스픽 대외활동",
            &p_campus,
        ) {
            eprintln!("[rss_write] campuspick failed: {e:?}");
        }
    }
    if !dacon_v.is_empty() {
        if let Err(e) = rss_write::write_rss_feed(
            &dacon_v,
            "DACON RSS",
            "https://www.dacon.io",
            "데이콘 대회",
            &p_dacon,
        ) {
            eprintln!("[rss_write] dacon failed: {e:?}");
        }
    }

    // ── URL 정규화 함수
    fn normalize_url(url: &str) -> String {
        if let Some((base, _)) = url.split_once("&gp=") {
            base.to_string()
        } else if let Some((base, _)) = url.split_once("?gp=") {
            base.to_string()
        } else {
            url.to_string()
        }
    }

    // ── 6) 통합용 벡터 만들기 + 중복 제거 + 정렬
    let mut all: Vec<Notice> =
        Vec::with_capacity(wevity_v.len() + campuspick_v.len() + dacon_v.len());
    all.extend(wevity_v.clone());
    all.extend(campuspick_v.clone());
    all.extend(dacon_v.clone());

    // 1차: URL 기준 중복 제거 (같은 플랫폼 내부 중복 제거)
    let mut seen_url = HashSet::new();
    all.retain(|n| seen_url.insert(normalize_url(&n.url)));

    // 2차: 플랫폼 간 중복 제거 (title + 기간 기준)
    let mut seen_cross = HashSet::new();
    all.retain(|n| {
        let key = format!(
            "{}|{}-{}",
            n.title.trim().to_lowercase(),
            n.start.as_ref().map(|d| d.to_string()).unwrap_or_default(),
            n.end.as_ref().map(|d| d.to_string()).unwrap_or_default()
        );
    seen_cross.insert(key)
});
    // 정렬
    all.sort_by(|a, b| {
        a.start.is_none().cmp(&b.start.is_none())
            .then(a.start.cmp(&b.start))
            .then(a.end.cmp(&b.end))
            .then(a.title.cmp(&b.title))
    });

    // ── 7) 통합 RSS 파일 생성
    if let Err(e) = rss_merged::write_merged_rss(
        vec![wevity_v, campuspick_v, dacon_v],
        "통합 공모전·대외활동 RSS",
        "https://wuisp-rust-dev.github.io/etc-crawler", 
        "모든 소식 통합",
        &p_merged,
    ) {
        eprintln!("[rss_merged] failed: {e:?}");
    }

    // ── 8) 콘솔 프리뷰
    println!("[Merged Notices: {} items]\n", all.len());
    for n in all.iter().take(preview_n) {
        println!("- {}", n);
    }

    eprintln!("[done]");
    Ok(())
}
