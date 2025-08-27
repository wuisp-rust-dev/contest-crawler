// src/notice.rs
use std::fmt;

#[derive(Clone, Debug)]
pub enum Source {
    Wevity,
    Dacon,
    Campuspick,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Contest,
    Activity,
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub source: Source,
    pub kind: Kind,                 // 공모전 / 대외활동
    pub title: String,
    pub url: String,
    pub start: Option<String>,      // YYYY-MM-DD
    pub end:   Option<String>,      // YYYY-MM-DD
    pub organizer: Option<String>,  // 주최/주관
    pub field: Option<String>,      // 분야(있으면)
}

pub fn infer_kind_from_label(label: &str, default: Kind) -> Kind {
    let s = label.trim().to_lowercase();
    if s.contains("활동") || s.contains("activity") {
        Kind::Activity
    } else if s.contains("공모") || s.contains("contest") || s.contains("competition") {
        Kind::Contest
    } else {
        default
    }
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let start = self.start.clone().unwrap_or_else(|| "-".into());
        let end   = self.end.clone().unwrap_or_else(|| "-".into());
        let org   = self.organizer.clone().unwrap_or_else(|| "-".into());
        let field = self.field.clone().unwrap_or_else(|| "-".into());

        write!(
            f,
            "[{:?}/{:?}] {} | {} | {} ~ {} | {}",
            self.source, self.kind, self.title, org, start, end, self.url
        )?;

        if !field.is_empty() && field != "-" {
            write!(f, " | {}", field)?;
        }
        Ok(())
    }
}