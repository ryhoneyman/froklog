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

fn game_display_name(id: &str) -> &str {
    match id {
        "eql" => "Everquest Legends",
        other => other,
    }
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

    // (game_id, server, player_name, StreamDisplay)
    let mut flat: Vec<(String, String, String, StreamDisplay)> = Vec::with_capacity(raw.len());
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
        flat.push((
            info.game,
            info.server,
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

    fn max_ts_of(streams: &[StreamDisplay]) -> Option<u64> {
        streams.iter().filter_map(|s| s.last_ts).max()
    }

    // Three-level grouping: game_key → server_key → player_key → streams.
    // Keys are normalized to lowercase so streams for the same player/server always
    // merge even if the client sent different capitalizations across sessions.
    // Separate display-name maps hold the first-seen raw string for each key.
    let mut by_game: HashMap<String, HashMap<String, HashMap<String, Vec<StreamDisplay>>>> =
        HashMap::new();
    let mut game_display: HashMap<String, String> = HashMap::new();
    let mut server_display: HashMap<(String, String), String> = HashMap::new();
    let mut player_display: HashMap<(String, String, String), String> = HashMap::new();

    for (game_id, server, player, display) in flat {
        let game_label = game_display_name(&game_id).to_string();
        let game_key = game_label.to_lowercase();
        let server_key = server.to_lowercase();
        let player_key = player.to_lowercase();

        game_display.entry(game_key.clone()).or_insert(game_label);
        server_display
            .entry((game_key.clone(), server_key.clone()))
            .or_insert(server);
        player_display
            .entry((game_key.clone(), server_key.clone(), player_key.clone()))
            .or_insert(player);

        by_game
            .entry(game_key)
            .or_default()
            .entry(server_key)
            .or_default()
            .entry(player_key)
            .or_default()
            .push(display);
    }

    // Convert to sorted Vec structure, each level sorted by max last_ts descending.
    type PlayerVec = Vec<(String, Vec<StreamDisplay>)>;
    type ServerVec = Vec<(String, PlayerVec)>;
    type GameVec = Vec<(String, ServerVec)>;

    let mut game_groups: GameVec = by_game
        .into_iter()
        .map(|(game_key, by_server)| {
            let game_label = game_display.remove(&game_key).unwrap_or_else(|| game_key.clone());
            let mut server_vec: ServerVec = by_server
                .into_iter()
                .map(|(server_key, by_player)| {
                    let server_label = server_display
                        .remove(&(game_key.clone(), server_key.clone()))
                        .unwrap_or_else(|| server_key.clone());
                    let mut player_vec: PlayerVec = by_player
                        .into_iter()
                        .map(|(player_key, mut streams)| {
                            let player_label = player_display
                                .remove(&(game_key.clone(), server_key.clone(), player_key.clone()))
                                .unwrap_or_else(|| player_key.clone());
                            streams.sort_by_key(|s| std::cmp::Reverse(s.last_ts));
                            (player_label, streams)
                        })
                        .collect();
                    player_vec.sort_by_key(|(_, streams)| std::cmp::Reverse(max_ts_of(streams)));
                    (server_label, player_vec)
                })
                .collect();
            server_vec.sort_by_key(|(_, pv)| {
                std::cmp::Reverse(pv.iter().filter_map(|(_, v)| max_ts_of(v)).max())
            });
            (game_label, server_vec)
        })
        .collect();
    game_groups.sort_by_key(|(_, sv)| {
        std::cmp::Reverse(
            sv.iter()
                .flat_map(|(_, pv)| pv.iter().filter_map(|(_, v)| max_ts_of(v)))
                .max(),
        )
    });

    let total_streams: usize = game_groups
        .iter()
        .flat_map(|(_, sv)| {
            sv.iter()
                .flat_map(|(_, pv)| pv.iter().map(|(_, v)| v.len()))
        })
        .sum();
    let total_players: usize = game_groups
        .iter()
        .flat_map(|(_, sv)| sv.iter().map(|(_, pv)| pv.len()))
        .sum();

    let mut body = String::new();
    for (gi, (game_label, server_groups)) in game_groups.iter().enumerate() {
        body.push_str(&format!(
            "<tr class=\"game-hdr\" data-gid=\"{gi}\" onclick=\"toggleGame({gi})\">\
              <td colspan=\"9\"><span class=\"chevron\">&#9660;</span>{}</td>\
            </tr>\n",
            html_escape(game_label)
        ));
        for (si, (server, player_groups)) in server_groups.iter().enumerate() {
            body.push_str(&format!(
                "<tr class=\"server-hdr\" data-gid=\"{gi}\" data-sid=\"{si}\" onclick=\"toggleServer({gi},{si})\">\
                  <td colspan=\"9\"><span class=\"chevron\">&#9660;</span>{}</td>\
                </tr>\n",
                html_escape(server)
            ));
            for (pi, (player, streams)) in player_groups.iter().enumerate() {
                body.push_str(&format!(
                    "<tr class=\"player-hdr\" data-gid=\"{gi}\" data-sid=\"{si}\" data-pid=\"{pi}\" onclick=\"togglePlayer({gi},{si},{pi})\">\
                      <td colspan=\"9\"><span class=\"chevron\">&#9660;</span>{}</td>\
                    </tr>\n",
                    html_escape(player)
                ));
                for s in streams {
                    let (row_class, status, status_kw) = if s.connected {
                        ("live-row", r#"<span class="live">&#9679; Live</span>"#, "live")
                    } else {
                        ("", r#"<span class="offline">&#9679; Offline</span>"#, "offline")
                    };
                    let view_url = format!("/stream/{}?vtok={}", s.stream_id, s.view_token);
                    let (public_badge, public_kw) = if s.public_stream {
                        (r#"<span class="pub-yes">&#10003; Yes</span>"#, "public")
                    } else {
                        (r#"<span class="pub-no">&#8212;</span>"#, "private")
                    };
                    let search_data = format!(
                        "{} {} {} {} {} {}",
                        s.stream_id.to_lowercase(),
                        player.to_lowercase(),
                        server.to_lowercase(),
                        game_label.to_lowercase(),
                        status_kw,
                        public_kw,
                    );
                    body.push_str(&format!(
                        "<tr class=\"stream-row {row_class}\" data-gid=\"{gi}\" data-sid=\"{si}\" data-pid=\"{pi}\" data-search=\"{}\">\
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
                        html_escape(&search_data),
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
        }
    }

    if body.is_empty() {
        body.push_str(r#"<tr><td colspan="9" class="empty">No streams yet.</td></tr>"#);
    }

    // Script lives outside the format! template to avoid escaping every brace.
    let script = r#"<script>
const collapsed = {};
function updateVisibility() {
    const q = document.getElementById('search').value.toLowerCase().trim();
    const searching = q.length > 0;
    const streamRows = [...document.querySelectorAll('tr.stream-row')];

    streamRows.forEach(row => {
        const {gid, sid, pid} = row.dataset;
        row.hidden = searching
            ? !row.dataset.search.includes(q)
            : !!(collapsed['g'+gid] || collapsed['s'+gid+'.'+sid] || collapsed['p'+gid+'.'+sid+'.'+pid]);
    });

    document.querySelectorAll('tr.player-hdr').forEach(row => {
        const {gid, sid, pid} = row.dataset;
        if (searching) {
            row.hidden = !streamRows.some(r => !r.hidden && r.dataset.gid===gid && r.dataset.sid===sid && r.dataset.pid===pid);
        } else {
            row.hidden = !!(collapsed['g'+gid] || collapsed['s'+gid+'.'+sid]);
        }
        row.querySelector('.chevron').textContent = collapsed['p'+gid+'.'+sid+'.'+pid] ? '▶' : '▼';
    });

    document.querySelectorAll('tr.server-hdr').forEach(row => {
        const {gid, sid} = row.dataset;
        if (searching) {
            row.hidden = !streamRows.some(r => !r.hidden && r.dataset.gid===gid && r.dataset.sid===sid);
        } else {
            row.hidden = !!collapsed['g'+gid];
        }
        row.querySelector('.chevron').textContent = collapsed['s'+gid+'.'+sid] ? '▶' : '▼';
    });

    document.querySelectorAll('tr.game-hdr').forEach(row => {
        const {gid} = row.dataset;
        if (searching) {
            row.hidden = !streamRows.some(r => !r.hidden && r.dataset.gid===gid);
        } else {
            row.hidden = false;
        }
        row.querySelector('.chevron').textContent = collapsed['g'+gid] ? '▶' : '▼';
    });

    const vis = streamRows.filter(r => !r.hidden).length;
    const tot = streamRows.length;
    const countEl = document.getElementById('count-line');
    if (searching) {
        countEl.textContent = vis + ' of ' + tot + ' stream(s) matching';
    } else {
        countEl.textContent = countEl.dataset.full;
    }
}
function toggleGame(gi) { collapsed['g'+gi] = !collapsed['g'+gi]; updateVisibility(); }
function toggleServer(gi,si) { const k='s'+gi+'.'+si; collapsed[k]=!collapsed[k]; updateVisibility(); }
function togglePlayer(gi,si,pi) { const k='p'+gi+'.'+si+'.'+pi; collapsed[k]=!collapsed[k]; updateVisibility(); }
document.addEventListener('DOMContentLoaded', () => {
    document.getElementById('search').addEventListener('input', updateVisibility);
});
</script>"#;

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
.top-bar{{display:flex;justify-content:space-between;align-items:flex-start;gap:1rem;margin-bottom:1rem}}
h1{{font-size:1.4rem;color:#89b4fa}}
h1 span{{color:#6c7086;font-weight:400;font-size:1rem;margin-left:.5rem}}
.count{{font-size:.85rem;color:#6c7086;margin-top:.3rem}}
#search{{background:#1e1e2e;color:#cdd6f4;border:1px solid #45475a;border-radius:6px;padding:.45rem .8rem;font-size:.85rem;width:280px;outline:none;margin-top:.1rem}}
#search:focus{{border-color:#89b4fa}}
#search::placeholder{{color:#6c7086}}
[hidden]{{display:none!important}}
table{{width:100%;border-collapse:collapse;background:#1e1e2e;border-radius:8px;overflow:hidden}}
th{{background:#313244;color:#a6adc8;font-size:.75rem;text-transform:uppercase;letter-spacing:.05em;padding:.6rem 1rem;text-align:left}}
td{{padding:.6rem 1rem;border-top:1px solid #313244;vertical-align:middle;white-space:nowrap}}
tr:hover td{{background:#262637}}
tr.game-hdr td{{background:#1a1a2e;color:#f38ba8;font-weight:700;font-size:.9rem;letter-spacing:.06em;text-transform:uppercase;border-top:3px solid #313244;padding:.55rem 1rem;cursor:pointer;user-select:none}}
tr.game-hdr:hover td{{background:#23233e}}
tr.server-hdr td{{background:#222233;color:#89dceb;font-weight:600;font-size:.82rem;letter-spacing:.05em;text-transform:uppercase;border-top:1px solid #313244;padding:.45rem 1rem .45rem 2rem;cursor:pointer;user-select:none}}
tr.server-hdr:hover td{{background:#2a2a3f}}
tr.player-hdr td{{background:#2a2a3d;color:#cba6f7;font-weight:600;font-size:.8rem;letter-spacing:.04em;text-transform:uppercase;border-top:1px solid #45475a;padding:.4rem 1rem .4rem 3rem;cursor:pointer;user-select:none}}
tr.player-hdr:hover td{{background:#333348}}
.chevron{{display:inline-block;font-size:.65rem;margin-right:.45rem;opacity:.6;vertical-align:middle}}
.mono{{font-family:monospace;font-size:.85rem;color:#a6e3a1}}
.ts{{font-size:.82rem;color:#a6adc8}}
.num{{font-size:.85rem;text-align:right}}
.dur{{color:#f9e2af}}
.live{{color:#a6e3a1;font-weight:600}}
.offline{{color:#6c7086}}
.pub-yes{{color:#89dceb;font-weight:600}}
.pub-no{{color:#45475a}}
.empty{{text-align:center;color:#6c7086;padding:2rem}}
tr.live-row td{{background:#1e2a1e}}
tr.live-row td:first-child{{border-left:3px solid #a6e3a1}}
tr.live-row:hover td{{background:#243024}}
a{{color:#89b4fa;text-decoration:none}}
a:hover{{text-decoration:underline}}
</style>
</head>
<body>
<div class="top-bar">
  <div>
    <h1>froklog <span>admin</span></h1>
    <p class="count" id="count-line" data-full="{total_streams} stream(s) across {total_players} player(s)">{total_streams} stream(s) across {total_players} player(s)</p>
  </div>
  <div>
    <input type="search" id="search" placeholder="Filter by player, server, stream ID, live, public&#8230;" autocomplete="off" spellcheck="false">
  </div>
</div>
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
{script}
</body>
</html>"#
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
