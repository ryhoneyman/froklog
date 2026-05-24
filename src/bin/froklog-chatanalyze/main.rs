//! froklog-chatanalyze — extract and correlate group/raid chat from EQ log files.
//!
//! Reads a raw EverQuest log and produces structured JSONL output by correlating
//! chat messages against nearby game events using a sliding time window.
//!
//! Modes:
//!   correlated  (default) — each chat line annotated with nearest game event
//!   corpus      — flat speaker+text pairs for fragment extraction
//!   stats       — per-speaker summary counts and timing distributions
//!
//! The log owner's lines ("You tell your party/raid, '...'") are attributed to
//! the player name derived from the filename (eqlog_<Name>_*) or --player-name.
//!
//! Usage:
//!   froklog-chatanalyze --input eqlog_Icestorm_test.txt
//!   froklog-chatanalyze --input eqlog.txt --player-name Icestorm --mode stats
//!   froklog-chatanalyze --input eqlog.txt --mode corpus --speaker Talodar

use std::cmp::Reverse;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use clap::{Parser, ValueEnum};
use serde::Serialize;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "froklog-chatanalyze",
    about = "Correlate group/raid chat with game events"
)]
struct Args {
    /// Input EQ log file
    #[arg(long, short)]
    input: PathBuf,

    /// Name of the log-owning player (defaults to extraction from filename)
    #[arg(long)]
    player_name: Option<String>,

    /// Lookback window in seconds for event correlation
    #[arg(long, default_value_t = 30)]
    window: u32,

    /// Output mode
    #[arg(long, default_value = "correlated")]
    mode: Mode,

    /// Filter to a single speaker (optional)
    #[arg(long)]
    speaker: Option<String>,

    /// Output file (default: stdout)
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(ValueEnum, Clone, Debug)]
enum Mode {
    Correlated,
    Corpus,
    Stats,
}

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum EventKind {
    Slay,
    PlayerDeath,
    Resist,
    ItemLoot,
    ExpGain,
}

#[derive(Debug, Clone)]
struct GameEvent {
    ts: i64,
    kind: EventKind,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum MsgClass {
    /// Chat fired within window seconds of a game event
    Reactive,
    /// Real-life meta: food/afk/phone/interruptions
    RealLife,
    /// Tactical: pull calls, CC, positioning
    Tactical,
    /// Loot bid callout: item name + bid number
    Loot,
    /// Everything else: banter, general social
    Social,
}

#[derive(Debug, Serialize)]
struct AnnotatedChat {
    ts: i64,
    speaker: String,
    channel: String,
    text: String,
    msg_class: MsgClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_kind: Option<EventKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_lag_secs: Option<i64>,
}

#[derive(Debug, Default)]
struct SpeakerStats {
    total: usize,
    reactive: usize,
    rl_meta: usize,
    tactical: usize,
    loot: usize,
    social: usize,
    total_len: usize,
    response_lags: Vec<i64>,
}

// ── Timestamp parsing ─────────────────────────────────────────────────────────

// "[Mon May 04 18:06:44 2026] " — 27 chars
const TS_LEN: usize = 27;

fn parse_ts(line: &str) -> Option<i64> {
    if line.len() < TS_LEN || !line.starts_with('[') {
        return None;
    }
    let ts_str = &line[1..25]; // "Mon May 04 18:06:44 2026"
                               // Try zero-padded day first (%d), fall back to space-padded (%e)
    NaiveDateTime::parse_from_str(ts_str, "%a %b %d %H:%M:%S %Y")
        .or_else(|_| NaiveDateTime::parse_from_str(ts_str, "%a %b %e %H:%M:%S %Y"))
        .ok()
        .map(|dt| dt.and_utc().timestamp())
}

// ── Player name extraction ────────────────────────────────────────────────────

/// Derive the log owner's name from an EQ log filename.
/// Pattern: eqlog_<Name>_<anything>.txt  →  Name
fn name_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    // stem: "eqlog_Icestorm_test-chat-training"
    let after = stem.strip_prefix("eqlog_")?;
    let name = after.split('_').next()?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

// ── Chat line parsing ─────────────────────────────────────────────────────────

struct ChatLine {
    speaker: String,
    channel: String,
    text: String,
}

fn parse_chat(body: &str, player_name: &str) -> Option<ChatLine> {
    // Log-owner: "You tell your party, 'text'" or "You tell your raid, 'text'"
    if let Some(rest) = body.strip_prefix("You tell your ") {
        let (channel_raw, rest) = rest.split_once(", '")?;
        let channel = match channel_raw {
            "party" => "group",
            "raid" => "raid",
            _ => return None,
        };
        let text = rest.strip_suffix('\'').unwrap_or(rest).to_string();
        return Some(ChatLine {
            speaker: player_name.to_string(),
            channel: channel.to_string(),
            text,
        });
    }

    // Other players: "Talodar tells the group, 'text'" or "... tells the raid, '...'"
    // Reject "voice-tells" lines — those have a space in the speaker token
    let (speaker, rest) = body.split_once(" tells the ")?;
    if speaker.contains(' ') {
        return None;
    }
    let (channel_raw, rest) = rest.split_once(", '")?;
    let channel = match channel_raw {
        "group" => "group",
        "raid" => "raid",
        _ => return None,
    };
    let text = rest.strip_suffix('\'').unwrap_or(rest).to_string();
    Some(ChatLine {
        speaker: speaker.to_string(),
        channel: channel.to_string(),
        text,
    })
}

// ── Game event detection ──────────────────────────────────────────────────────

fn classify_event(body: &str) -> Option<(EventKind, String)> {
    // Slay: "X has been slain by Y!" or "You have slain X!"
    if let Some(idx) = body.find(" has been slain by ") {
        return Some((EventKind::Slay, body[..idx].to_string()));
    }
    if let Some(rest) = body.strip_prefix("You have slain ") {
        let mob = rest.trim_end_matches('!').to_string();
        return Some((EventKind::Slay, mob));
    }
    // Player death
    if let Some(rest) = body.strip_prefix("You have been slain by ") {
        let by = rest.trim_end_matches('!').to_string();
        return Some((EventKind::PlayerDeath, by));
    }
    if body.ends_with(" died.") {
        return Some((
            EventKind::PlayerDeath,
            body.trim_end_matches(" died.").to_string(),
        ));
    }
    // Resist
    if body.contains(" resisted ") || body.starts_with("You resist ") {
        return Some((EventKind::Resist, String::new()));
    }
    // Item loot
    if let Some(rest) = body.strip_prefix("--You have looted ") {
        let item = rest.split(" from").next().unwrap_or("").to_string();
        return Some((EventKind::ItemLoot, item));
    }
    if body.starts_with("You receive ") && body.contains(" from ") {
        let item = body["You receive ".len()..]
            .split(" from")
            .next()
            .unwrap_or("")
            .to_string();
        return Some((EventKind::ItemLoot, item));
    }
    // Experience gain
    if body.starts_with("You gain") && body.contains("experience") {
        return Some((EventKind::ExpGain, String::new()));
    }
    None
}

// ── Message classification ────────────────────────────────────────────────────

const RL_KEYWORDS: &[&str] = &[
    "brb",
    "afk",
    "back",
    "bio",
    "bathroom",
    "phone",
    "irl",
    "food",
    "snack",
    "drink",
    "water",
    "eat",
    "pizza",
    "waffle",
    "kitchen",
    "wife",
    "gf",
    "boyfriend",
    "husband",
    "kid",
    "dog",
    "cat",
    "baby",
    "stepping out",
    "gonna grab",
    "gotta go",
    "distract",
    "interrupt",
    "someone at the",
    "at the door",
];

const TACTICAL_KEYWORDS: &[&str] = &[
    "incoming",
    "inc",
    "pulling",
    "pull",
    "slow",
    "mezz",
    "mez",
    "mezzing",
    "root",
    "rooting",
    "charm",
    "fd",
    "feign",
    "camp",
    "safe",
    "clear",
    "assist",
    "adds",
    "add",
    "train",
    "aggro",
    "agro",
    "snare",
    "kite",
    "burn",
    "main assist",
    "don't break",
    "dont break",
    "hold cc",
];

fn classify_message(text: &str, has_recent_event: bool) -> MsgClass {
    let lower = text.to_lowercase();

    // Loot bid: "Item Name +N" or "Item Name -N"
    // Short message (2–7 words), last token is a signed integer, first word capitalized
    let words: Vec<&str> = lower.split_whitespace().collect();
    if words.len() >= 2 && words.len() <= 7 {
        let last = *words.last().unwrap();
        let num_part = last.trim_start_matches(['+', '-']);
        if !num_part.is_empty() && num_part.chars().all(|c| c.is_ascii_digit()) {
            let orig_first = text.split_whitespace().next().unwrap_or("");
            if orig_first
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
            {
                return MsgClass::Loot;
            }
        }
    }

    for kw in RL_KEYWORDS {
        if lower == *kw
            || lower.starts_with(&format!("{kw} "))
            || lower.ends_with(&format!(" {kw}"))
            || lower.contains(&format!(" {kw} "))
        {
            return MsgClass::RealLife;
        }
    }

    for kw in TACTICAL_KEYWORDS {
        if lower == *kw
            || lower.starts_with(&format!("{kw} "))
            || lower.ends_with(&format!(" {kw}"))
            || lower.contains(&format!(" {kw} "))
        {
            return MsgClass::Tactical;
        }
    }

    if has_recent_event {
        return MsgClass::Reactive;
    }

    MsgClass::Social
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() -> io::Result<()> {
    let args = Args::parse();

    // Resolve the log owner's name
    let player_name = args
        .player_name
        .clone()
        .or_else(|| name_from_filename(&args.input))
        .unwrap_or_else(|| "You".to_string());

    let file = File::open(&args.input)?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut out: Box<dyn Write> = match &args.output {
        Some(p) => Box::new(BufWriter::new(File::create(p)?)),
        None => Box::new(BufWriter::new(io::stdout().lock())),
    };

    let window_secs = args.window as i64;
    let mut recent_events: VecDeque<GameEvent> = VecDeque::new();
    let mut stats: HashMap<String, SpeakerStats> = HashMap::new();

    for line in reader.lines() {
        let line = line?;
        if line.len() < TS_LEN {
            continue;
        }
        let ts = match parse_ts(&line) {
            Some(t) => t,
            None => continue,
        };
        let body = &line[TS_LEN..];

        if let Some(chat) = parse_chat(body, &player_name) {
            if let Some(ref spk) = args.speaker {
                if chat.speaker != *spk {
                    continue;
                }
            }

            // Drop stale events
            while recent_events
                .front()
                .map(|e| ts - e.ts > window_secs)
                .unwrap_or(false)
            {
                recent_events.pop_front();
            }

            let recent = recent_events.back();
            let has_recent = recent.is_some();
            let lag = recent.map(|e| ts - e.ts);
            let msg_class = classify_message(&chat.text, has_recent);

            match args.mode {
                Mode::Correlated => {
                    let record = AnnotatedChat {
                        ts,
                        speaker: chat.speaker,
                        channel: chat.channel,
                        text: chat.text,
                        msg_class,
                        event_kind: recent.map(|e| e.kind.clone()),
                        event_detail: recent.map(|e| e.detail.clone()),
                        event_lag_secs: lag,
                    };
                    serde_json::to_writer(&mut out, &record)?;
                    writeln!(out)?;
                }
                Mode::Corpus => {
                    let record = serde_json::json!({
                        "speaker": chat.speaker,
                        "channel": chat.channel,
                        "text": chat.text,
                    });
                    serde_json::to_writer(&mut out, &record)?;
                    writeln!(out)?;
                }
                Mode::Stats => {
                    let entry = stats.entry(chat.speaker).or_default();
                    entry.total += 1;
                    entry.total_len += chat.text.len();
                    match &msg_class {
                        MsgClass::Reactive => {
                            entry.reactive += 1;
                            if let Some(l) = lag {
                                entry.response_lags.push(l);
                            }
                        }
                        MsgClass::RealLife => entry.rl_meta += 1,
                        MsgClass::Tactical => entry.tactical += 1,
                        MsgClass::Loot => entry.loot += 1,
                        MsgClass::Social => entry.social += 1,
                    }
                }
            }
        } else if let Some((kind, detail)) = classify_event(body) {
            while recent_events
                .front()
                .map(|e| ts - e.ts > window_secs)
                .unwrap_or(false)
            {
                recent_events.pop_front();
            }
            recent_events.push_back(GameEvent { ts, kind, detail });
        }
    }

    // Emit stats after full pass
    if matches!(args.mode, Mode::Stats) {
        let mut speakers: Vec<(&String, &SpeakerStats)> = stats.iter().collect();
        speakers.sort_by_key(|a| Reverse(a.1.total));

        for (speaker, s) in speakers {
            let avg_len = if s.total > 0 {
                s.total_len as f64 / s.total as f64
            } else {
                0.0
            };
            let median_lag = if !s.response_lags.is_empty() {
                let mut sorted = s.response_lags.clone();
                sorted.sort_unstable();
                sorted[sorted.len() / 2]
            } else {
                0
            };
            let record = serde_json::json!({
                "speaker": speaker,
                "total": s.total,
                "reactive": s.reactive,
                "rl_meta": s.rl_meta,
                "tactical": s.tactical,
                "loot": s.loot,
                "social": s.social,
                "avg_msg_len": format!("{:.1}", avg_len),
                "median_reaction_secs": median_lag,
            });
            serde_json::to_writer(&mut out, &record)?;
            writeln!(out)?;
        }
    }

    Ok(())
}
