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

struct SessionDisplay {
    num: u32,
    label: String,
    start_log_ts: u64,
    end_log_ts: Option<u64>,
    duration_secs: Option<u64>,
    batch_count: usize,
}

struct StreamDisplay {
    stream_id: String,
    game_id: String,
    view_token: String,
    connected: bool,
    public_stream: bool,
    is_replay: bool,
    first_ts: Option<u64>,
    last_ts: Option<u64>,
    batches: usize,
    file_bytes: u64,
    duration_secs: Option<u64>,
    sessions: Vec<SessionDisplay>,
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
        let si = info.session_index.read().await;
        let sessions_raw = si.list().to_vec();
        drop(si);
        let journal = info.journal.read().await;
        let batches = journal.len();
        let first_ts = journal.log_first_ts();
        let last_ts = journal.log_last_ts();
        // Sessions anchor to permanent batch ids; derive positional offsets
        // for the per-session batch counts.
        let session_pos: Vec<usize> = sessions_raw
            .iter()
            .map(|s| journal.pos_of_id(s.start_batch_id))
            .collect();
        drop(journal);
        let file_bytes = std::fs::metadata(&info.journal_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let duration_secs = first_ts.zip(last_ts).map(|(f, l)| l.saturating_sub(f));
        let sessions: Vec<SessionDisplay> = sessions_raw
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let batch_count = if i + 1 < sessions_raw.len() {
                    session_pos[i + 1].saturating_sub(session_pos[i])
                } else {
                    batches.saturating_sub(session_pos[i])
                };
                let end_log_ts = if i + 1 < sessions_raw.len() {
                    Some(sessions_raw[i + 1].start_log_ts)
                } else {
                    last_ts
                };
                SessionDisplay {
                    num: s.num,
                    label: s.label.clone(),
                    start_log_ts: s.start_log_ts,
                    end_log_ts,
                    duration_secs: end_log_ts.map(|e| e.saturating_sub(s.start_log_ts)),
                    batch_count,
                }
            })
            .collect();
        flat.push((
            info.game.clone(),
            info.server,
            info.player_name,
            StreamDisplay {
                stream_id: info.stream_id,
                game_id: info.game,
                view_token: info.view_token,
                connected: info.connected,
                public_stream: info.public_stream,
                is_replay: info.is_replay,
                first_ts,
                last_ts,
                batches,
                file_bytes,
                duration_secs,
                sessions,
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
            let game_label = game_display
                .remove(&game_key)
                .unwrap_or_else(|| game_key.clone());
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

    let icon_view = r#"<svg xmlns="http://www.w3.org/2000/svg" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/><circle cx="10" cy="15" r="2.5"/><line x1="12.2" y1="17.2" x2="14.5" y2="19.5"/></svg>"#;
    let icon_reset = r#"<svg xmlns="http://www.w3.org/2000/svg" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="1 4 1 10 7 10"/><path d="M3.51 15a9 9 0 1 0 .49-3.01"/></svg>"#;
    let icon_delete = r#"<svg xmlns="http://www.w3.org/2000/svg" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2"/></svg>"#;

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
                    let (row_class, status, status_kw) = if s.is_replay {
                        (
                            "recorded-row",
                            r#"<span class="recorded">&#9679; Recorded</span>"#,
                            "recorded",
                        )
                    } else if s.connected {
                        (
                            "live-row",
                            r#"<span class="live">&#9679; Live</span>"#,
                            "live",
                        )
                    } else {
                        (
                            "",
                            r#"<span class="offline">&#9679; Offline</span>"#,
                            "offline",
                        )
                    };
                    let view_url = if s.public_stream {
                        format!("/player/{}/{}/{}", s.game_id, server, player)
                    } else {
                        format!("/stream/{}?vtok={}", s.stream_id, s.view_token)
                    };
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
                    let stream_id_cell = if s.sessions.len() <= 1 {
                        format!("<td class=\"mono\"><span class=\"chevron\" style=\"visibility:hidden\">&#9660;</span>{}</td>", s.stream_id)
                    } else {
                        format!(
                            "<td class=\"mono has-sessions\" onclick=\"toggleSessions('{}')\">\
<span class=\"chevron\" id=\"chev-{}\">&#9660;</span>{} ({})\
</td>",
                            s.stream_id,
                            s.stream_id,
                            s.stream_id,
                            s.sessions.len()
                        )
                    };
                    let actions_cell = format!(
                        "<td class=\"act\">\
                          <a href=\"{}\" target=\"_blank\" class=\"btn-act btn-view\" title=\"View stream\">{icon_view}</a>\
                          <button class=\"btn-act btn-reset\" onclick=\"resetStream('{}')\" title=\"Reset: erase journal and sessions\">{icon_reset}</button>\
                          <button class=\"btn-act btn-delete\" onclick=\"deleteStream('{}')\" title=\"Delete stream permanently\">{icon_delete}</button>\
                        </td>",
                        html_escape(&view_url),
                        s.stream_id,
                        s.stream_id,
                    );
                    body.push_str(&format!(
                        "<tr class=\"stream-row {row_class}\" data-gid=\"{gi}\" data-sid=\"{si}\" data-pid=\"{pi}\" data-stid=\"{}\" data-search=\"{}\">\
                          {}\
                          <td class=\"ts\">{}</td>\
                          <td class=\"ts\">{}</td>\
                          <td class=\"num dur\">{}</td>\
                          <td class=\"num\">{}</td>\
                          <td class=\"num\">{}</td>\
                          <td>{}</td>\
                          <td>{}</td>\
                          {actions_cell}\
                        </tr>\n",
                        s.stream_id,
                        html_escape(&search_data),
                        stream_id_cell,
                        format_ts(s.first_ts),
                        format_ts(s.last_ts),
                        format_duration(s.duration_secs),
                        s.batches,
                        format_size(s.file_bytes),
                        status,
                        public_badge,
                    ));
                    if s.sessions.len() > 1 {
                        for session in &s.sessions {
                            body.push_str(&format!(
                                "<tr class=\"session-row\" data-stid=\"{}\" hidden>\
                                  <td class=\"session-num\">\u{21B3}&ensp;#{} &mdash; {}</td>\
                                  <td class=\"ts\">{}</td>\
                                  <td class=\"ts\">{}</td>\
                                  <td class=\"num dur\">{}</td>\
                                  <td class=\"num\">{}</td>\
                                  <td></td><td></td><td></td><td></td>\
                                </tr>\n",
                                s.stream_id,
                                session.num,
                                html_escape(&session.label),
                                format_ts(Some(session.start_log_ts)),
                                format_ts(session.end_log_ts),
                                format_duration(session.duration_secs),
                                session.batch_count,
                            ));
                        }
                    }
                }
            }
        }
    }

    if body.is_empty() {
        body.push_str(r#"<tr><td colspan="9" class="empty">No streams yet.</td></tr>"#);
    }

    // Script lives outside the format! template to avoid escaping every brace.
    let script = r#"<script>
let _autoRefreshTimer = null;
function setAutoRefresh(on) {
    clearInterval(_autoRefreshTimer);
    const btn = document.getElementById('auto-refresh-btn');
    if (on) {
        _autoRefreshTimer = setInterval(() => location.reload(), 30000);
        btn.textContent = 'Auto-refresh: ON';
        btn.style.color = '#a6e3a1';
        sessionStorage.setItem('adminAutoRefresh', '1');
    } else {
        btn.textContent = 'Auto-refresh: OFF';
        btn.style.color = '';
        sessionStorage.removeItem('adminAutoRefresh');
    }
}
const collapsed = {};
const expandedStreams = {};
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

    // Session rows follow their parent stream row's visibility and expand state.
    document.querySelectorAll('tr.session-row').forEach(row => {
        const stid = row.dataset.stid;
        const parent = document.querySelector('tr.stream-row[data-stid="' + stid + '"]');
        row.hidden = !parent || parent.hidden || !expandedStreams[stid];
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
function toggleSessions(id) {
    expandedStreams[id] = !expandedStreams[id];
    updateVisibility();
    const chev = document.getElementById('chev-' + id);
    if (chev) chev.textContent = expandedStreams[id] ? '▼' : '▶';
}
async function resetStream(id) {
    if (!confirm('Reset stream ' + id + '?\n\nThis will permanently erase all journal data and sessions.\nThe stream identity and tokens will be preserved.')) return;
    const atok = new URLSearchParams(window.location.search).get('atok');
    const resp = await fetch('/stream/' + id, {
        method: 'DELETE',
        headers: { 'Authorization': 'Bearer ' + atok }
    });
    if (resp.ok) {
        location.reload();
    } else {
        alert('Reset failed: HTTP ' + resp.status);
    }
}
async function deleteStream(id) {
    if (!confirm('Delete stream ' + id + '?\n\nThis will permanently remove the stream, all journal data, sessions, and the stream identity.\nThis cannot be undone.')) return;
    const atok = new URLSearchParams(window.location.search).get('atok');
    const resp = await fetch('/stream/' + id + '/purge', {
        method: 'DELETE',
        headers: { 'Authorization': 'Bearer ' + atok }
    });
    if (resp.ok) {
        location.reload();
    } else {
        alert('Delete failed: HTTP ' + resp.status);
    }
}
document.addEventListener('DOMContentLoaded', () => {
    document.getElementById('search').addEventListener('input', updateVisibility);
    if (sessionStorage.getItem('adminAutoRefresh')) setAutoRefresh(true);
});
</script>"#;

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>froklog — Admin</title>
<link rel="icon" type="image/png" href="/favicon.ico">
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
.recorded{{color:#cba6f7;font-weight:600}}
.pub-yes{{color:#89dceb;font-weight:600}}
.pub-no{{color:#45475a}}
.empty{{text-align:center;color:#6c7086;padding:2rem}}
tr.live-row td{{background:#1e2a1e}}
tr.live-row td:first-child{{border-left:3px solid #a6e3a1}}
tr.recorded-row td{{background:#1e1a2a}}
tr.recorded-row td:first-child{{border-left:3px solid #cba6f7}}
tr.live-row:hover td{{background:#243024}}
a{{color:#89b4fa;text-decoration:none}}
a:hover{{text-decoration:underline}}
.btn-act{{background:none;border:none;cursor:pointer;padding:2px;vertical-align:middle;line-height:0;border-radius:4px;opacity:.75;transition:opacity .15s,background .15s}}
.btn-act:hover{{opacity:1;background:#313244}}
.btn-view{{color:#89b4fa}}
.btn-reset{{color:#f9e2af}}
.btn-delete{{color:#f38ba8}}
.act{{white-space:nowrap}}
.act .btn-act+.btn-act{{margin-left:2px}}
.btn-sessions{{background:none;border:none;color:#89b4fa;cursor:pointer;font-size:.78rem;padding:0 0 0 .3rem;font-family:inherit;opacity:.7}}
.btn-sessions:hover{{opacity:1;text-decoration:underline}}
tr.session-row td{{background:#15151f;font-size:.78rem;color:#6c7086;border-top:1px solid #1e1e2e}}
tr.stream-row td:first-child{{padding-left:4rem}}
tr.stream-row td.has-sessions{{cursor:pointer;user-select:none}}
tr.session-row td:first-child{{border-left:2px solid #313244;padding-left:5rem;color:#a6adc8}}
tr.session-row:hover td{{background:#1a1a2a}}
.session-num{{font-style:italic}}
</style>
</head>
<body>
<div class="top-bar">
  <div>
    <h1>froklog <span>admin</span></h1>
    <p class="count" id="count-line" data-full="{total_streams} stream(s) across {total_players} player(s)">{total_streams} stream(s) across {total_players} player(s)</p>
  </div>
  <div style="display:flex;align-items:center;gap:.6rem">
    <input type="search" id="search" placeholder="Filter by player, server, stream ID, live, recorded, public&#8230;" autocomplete="off" spellcheck="false">
    <button id="auto-refresh-btn" onclick="setAutoRefresh(!_autoRefreshTimer)" style="background:#1e1e2e;color:#6c7086;border:1px solid #45475a;border-radius:6px;padding:.45rem .8rem;font-size:.85rem;cursor:pointer;white-space:nowrap;font-family:inherit">Auto-refresh: OFF</button>
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
        .replace('\'', "&#39;")
}
