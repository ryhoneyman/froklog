use std::collections::HashMap;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use tracing::{info, warn};

use crate::ServerState;

#[derive(Deserialize)]
pub struct AdminQuery {
    pub atok: Option<String>,
}

struct StreamDisplay {
    stream_id: String,
    view_token: String,
    connected: bool,
    public_stream: bool,
    first_ts: Option<u64>,
    last_ts: Option<u64>,
    batches: usize,
    file_bytes: u64,
    duration_secs: Option<u64>,
}

/// `GET /admin?atok=<admin_token>` — lists all streams grouped by player.
pub async fn admin_panel_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(params): Query<AdminQuery>,
    State(state): State<ServerState>,
) -> Response {
    let ip = crate::client_ip(&headers, peer);
    let token = match params.atok.as_deref() {
        Some(t) if !t.is_empty() => t,
        _ => {
            warn!("Admin [{ip}]: rejected — missing token");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };
    if !state.is_admin_token(token) {
        warn!("Admin [{ip}]: rejected — bad token");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    info!("Admin [{ip}]: access granted");

    // Release registry lock before acquiring any journal locks.
    let raw = state.registry.read().await.list_admin();

    let mut displays: Vec<(String, StreamDisplay)> = Vec::with_capacity(raw.len());
    for info in raw {
        let journal = info.journal.read().await;
        let batches = journal.len();
        let first_ts = journal.log_first_ts();
        let last_ts = journal.log_last_ts();
        drop(journal);
        let file_bytes = std::fs::metadata(&info.journal_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let duration_secs = first_ts.zip(last_ts).map(|(f, l)| l.saturating_sub(f));
        displays.push((
            info.player_name,
            StreamDisplay {
                stream_id: info.stream_id,
                view_token: info.view_token,
                connected: info.connected,
                public_stream: info.public_stream,
                first_ts,
                last_ts,
                batches,
                file_bytes,
                duration_secs,
            },
        ));
    }

    // Group by player name.
    let mut by_player: HashMap<String, Vec<StreamDisplay>> = HashMap::new();
    for (player, display) in displays {
        by_player.entry(player).or_default().push(display);
    }

    // Sort each group's streams by last_ts descending (no data → bottom).
    for streams in by_player.values_mut() {
        streams.sort_by_key(|b| std::cmp::Reverse(b.last_ts));
    }

    // Sort player groups by their most recent last_ts descending.
    let mut groups: Vec<(String, Vec<StreamDisplay>)> = by_player.into_iter().collect();
    groups.sort_by(|a, b| {
        let a_ts = a.1.iter().filter_map(|s| s.last_ts).max();
        let b_ts = b.1.iter().filter_map(|s| s.last_ts).max();
        b_ts.cmp(&a_ts)
    });

    let total_streams: usize = groups.iter().map(|(_, v)| v.len()).sum();

    let mut body = String::new();
    for (player, streams) in &groups {
        body.push_str(&format!(
            "<tr class=\"player-hdr\"><td colspan=\"9\">{}</td></tr>\n",
            html_escape(player)
        ));
        for s in streams {
            let (row_class, status) = if s.connected {
                ("live-row", r#"<span class="live">&#9679; Live</span>"#)
            } else {
                ("", r#"<span class="offline">&#9679; Offline</span>"#)
            };
            let view_url = format!("/stream/{}?vtok={}", s.stream_id, s.view_token);
            let public_badge = if s.public_stream {
                r#"<span class="pub-yes">&#10003; Yes</span>"#
            } else {
                r#"<span class="pub-no">&#8212;</span>"#
            };
            body.push_str(&format!(
                "<tr class=\"{row_class}\">\
                  <td class=\"mono\">{}</td>\
                  <td class=\"ts\">{}</td>\
                  <td class=\"ts\">{}</td>\
                  <td class=\"num dur\">{}</td>\
                  <td class=\"num\">{}</td>\
                  <td class=\"num\">{}</td>\
                  <td>{}</td>\
                  <td>{}</td>\
                  <td><a href=\"{}\" target=\"_blank\">View</a></td>\
                </tr>\n",
                s.stream_id,
                format_ts(s.first_ts),
                format_ts(s.last_ts),
                format_duration(s.duration_secs),
                s.batches,
                format_size(s.file_bytes),
                status,
                public_badge,
                html_escape(&view_url),
            ));
        }
    }

    if body.is_empty() {
        body.push_str(r#"<tr><td colspan="9" class="empty">No streams yet.</td></tr>"#);
    }

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>froklog — Admin</title>
<style>
*{{box-sizing:border-box;margin:0;padding:0}}
body{{background:#0f1117;color:#cdd6f4;font-family:'Segoe UI',system-ui,sans-serif;padding:2rem}}
h1{{font-size:1.4rem;margin-bottom:1rem;color:#89b4fa}}
h1 span{{color:#6c7086;font-weight:400;font-size:1rem;margin-left:.5rem}}
.count{{font-size:.85rem;color:#6c7086;margin-bottom:1rem}}
table{{width:100%;border-collapse:collapse;background:#1e1e2e;border-radius:8px;overflow:hidden}}
th{{background:#313244;color:#a6adc8;font-size:.75rem;text-transform:uppercase;letter-spacing:.05em;padding:.6rem 1rem;text-align:left}}
td{{padding:.6rem 1rem;border-top:1px solid #313244;vertical-align:middle;white-space:nowrap}}
tr:hover td{{background:#262637}}
tr.player-hdr td{{background:#2a2a3d;color:#cba6f7;font-weight:600;font-size:.85rem;letter-spacing:.04em;text-transform:uppercase;border-top:2px solid #45475a;padding:.5rem 1rem}}
.mono{{font-family:monospace;font-size:.85rem;color:#a6e3a1}}
.ts{{font-size:.82rem;color:#a6adc8}}
.num{{font-size:.85rem;text-align:right}}
.dur{{color:#f9e2af}}
.live{{color:#a6e3a1;font-weight:600}}
.offline{{color:#6c7086}}
.pub-yes{{color:#89dceb;font-weight:600}}
.pub-no{{color:#45475a}}
.empty{{text-align:center;color:#6c7086;padding:2rem}}
tr.live-row td{{background:#1e2a1e;border-left:3px solid #a6e3a1}}
tr.live-row:hover td{{background:#243024}}
a{{color:#89b4fa;text-decoration:none}}
a:hover{{text-decoration:underline}}
</style>
</head>
<body>
<h1>froklog <span>admin</span></h1>
<p class="count">{total_streams} stream(s) across {} player(s)</p>
<table>
  <thead>
    <tr>
      <th>Stream ID</th>
      <th>First Seen</th>
      <th>Last Activity</th>
      <th style="text-align:right">Duration</th>
      <th style="text-align:right">Batches</th>
      <th style="text-align:right">Size</th>
      <th>Status</th>
      <th>Public</th>
      <th>Actions</th>
    </tr>
  </thead>
  <tbody>
{body}  </tbody>
</table>
</body>
</html>"#,
        groups.len()
    );

    Html(html).into_response()
}

fn format_duration(secs: Option<u64>) -> String {
    match secs {
        None => "\u{2014}".to_string(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        Some(s) if s < 86400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        Some(s) => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
    }
}

fn format_ts(ts: Option<u64>) -> String {
    match ts {
        None => "\u{2014}".to_string(),
        Some(t) => {
            use chrono::TimeZone;
            chrono::Utc
                .timestamp_opt(t as i64, 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "\u{2014}".to_string())
        }
    }
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1_048_576.0))
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
