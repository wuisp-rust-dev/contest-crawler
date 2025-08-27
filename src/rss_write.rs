// src/rss_write.rs
use rss::{ChannelBuilder, ItemBuilder, CategoryBuilder};
use std::fs::File;
use std::io::Write;
use chrono::{NaiveDate, Datelike, Utc, TimeZone};
use anyhow::Result;

use crate::notice::{Notice, Kind}; 

pub fn write_rss_feed(
    notices: &[Notice],
    channel_title: &str,
    channel_link: &str,
    channel_desc: &str,
    output_file: &str,
) -> Result<()> {
    let items = notices.iter().map(|n| {
        // pubDate: start → end → now
        let pub_date = n
            .start.as_ref()
            .and_then(|d| ymd_to_rfc2822(d))
            .or_else(|| n.end.as_ref().and_then(|d| ymd_to_rfc2822(d)))
            .or_else(|| Some(Utc::now().to_rfc2822()));

        // 본문
        let description = format!(
            "주최: {}<br>기간: {} ~ {}<br>분야: {}",
            n.organizer.as_deref().unwrap_or("-"),
            n.start.as_deref().unwrap_or("-"),
            n.end.as_deref().unwrap_or("-"),
            n.field.as_deref().unwrap_or("-")
        );

        // category: kind + source (enum → 라벨)
        let kind_label = match &n.kind {
            Kind::Contest  => "공모전",
            Kind::Activity => "대외활동",
        };
        // Source는 Debug가 파생되어 있다고 가정(기존 코드 동일)
        let source_label = format!("{:?}", n.source);

        let categories = vec![
            CategoryBuilder::default().name(kind_label.to_string()).build(),
            CategoryBuilder::default().name(source_label).build(),
        ];

        ItemBuilder::default()
            .title(Some(n.title.clone()))
            .link(Some(n.url.clone()))
            .description(Some(description))
            .pub_date(pub_date)
            .categories(categories)
            .build()
    }).collect::<Vec<_>>();

    let channel = ChannelBuilder::default()
        .title(channel_title)
        .link(channel_link)
        .description(channel_desc)
        .items(items)
        .build();


    let mut file = File::create(output_file)?;
    file.write_all(channel.to_string().as_bytes())?;
    Ok(())
}

fn ymd_to_rfc2822(ymd: &str) -> Option<String> {
    let date = NaiveDate::parse_from_str(ymd, "%Y-%m-%d").ok()?;
    let dt = Utc.with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0).single()?;
    Some(dt.to_rfc2822())
}
