//! Mining the player's own log for trigger material.
//!
//! Writing a sound-trigger regex needs a real example line, and nobody
//! remembers exactly how the game phrases a crit or a stun. Two tools:
//! substring search (find the line you half-remember) and a unique-template
//! scan (surface every kind of non-chat message the log actually contains,
//! deduplicated). Both exclude chat — triggers are for combat/system lines.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// How much of the log tail gets scanned. Enough for days of play.
const SCAN_BYTES: u64 = 8 * 1024 * 1024;

/// Player chat in every channel form. Kept deliberately broad: a missed
/// combat line is a minor gap, chat leaking into trigger seeds is noise.
fn is_chat(body: &str) -> bool {
    for pat in [
        " says, '",
        " says '",
        " tells ",
        " told ",
        "You say, '",
        "You say '",
        " shouts, '",
        " shouts '",
        " auctions, '",
        " ooc, '",
        " out of character, '",
        " group, '",
        " guild, '",
        "You tell ",
        "You shout",
        "You auction",
        "You told",
        " says out of character",
    ] {
        if body.contains(pat) {
            return true;
        }
    }
    false
}

fn read_tail(log: &Path) -> Option<String> {
    let mut f = std::fs::File::open(log).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(SCAN_BYTES)))
        .ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Strip the "[Thu Jul 31 ...] " timestamp prefix.
fn body(line: &str) -> &str {
    if line.len() > froklog::patterns::TS_LEN && line.starts_with('[') {
        &line[froklog::patterns::TS_LEN..]
    } else {
        line
    }
}

/// Case-insensitive substring search over the log tail, chat excluded,
/// deduplicated, newest first.
pub fn search(log: &Path, needle: &str, max: usize) -> Vec<String> {
    let Some(text) = read_tail(log) else {
        return Vec::new();
    };
    let needle = needle.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in text.lines().rev() {
        let b = body(line);
        if b.is_empty() || is_chat(b) || !b.to_lowercase().contains(&needle) {
            continue;
        }
        if seen.insert(b.to_string()) {
            out.push(b.to_string());
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

/// The log's unique non-chat message SHAPES: digits collapsed so "for 37
/// points" and "for 214 points" are one template, counted, most common
/// first. Each template is represented by its most recent real line.
pub fn unique_templates(log: &Path, max: usize) -> Vec<(u32, String)> {
    let Some(text) = read_tail(log) else {
        return Vec::new();
    };
    // template key -> (count, most recent example)
    let mut map: std::collections::HashMap<String, (u32, String)> =
        std::collections::HashMap::new();
    for line in text.lines() {
        let b = body(line);
        if b.is_empty() || is_chat(b) {
            continue;
        }
        let mut key = String::with_capacity(b.len());
        let mut in_num = false;
        for c in b.chars() {
            if c.is_ascii_digit() {
                if !in_num {
                    key.push('#');
                    in_num = true;
                }
            } else {
                in_num = false;
                key.push(c);
            }
        }
        let e = map.entry(key).or_insert((0, String::new()));
        e.0 += 1;
        e.1 = b.to_string();
    }
    let mut out: Vec<(u32, String)> = map.into_values().collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.truncate(max);
    out
}

/// The message shapes the parser understands, each with a realistic example
/// — the "what does a crit even look like" cheat sheet. Click-to-seed into
/// the trigger builder. Examples use this server's observed formats.
pub const KNOWN_SHAPES: &[(&str, &str)] = &[
    (
        "melee hit",
        "You slash an orc pawn for 36 points of damage.",
    ),
    (
        "critical hit (suffix!)",
        "You cleave Emperor Crush for 37 points of damage. (Critical)",
    ),
    (
        "mob hits you",
        "An orc pawn hits YOU for 12 points of damage.",
    ),
    (
        "you avoid",
        "Emperor Crush tries to slash YOU, but YOU dodge!",
    ),
    (
        "mob avoids",
        "You try to slash an orc pawn, but an orc pawn dodges!",
    ),
    (
        "spell damage",
        "Ruin hit an orc pawn for 44 points of fire damage by Bolt of Flame.",
    ),
    (
        "damage over time",
        "An orc pawn has been damaged by Zary's Flame Lick for 8 points of damage.",
    ),
    (
        "heal",
        "Izzin healed Zary for 120 hit points by Light Healing.",
    ),
    ("you kill", "You have slain Emperor Crush!"),
    ("someone kills", "Ruin has slain an orc pawn!"),
    ("you die", "You have been slain by an orc pawn!"),
    ("spell cast starts", "Ruin begins casting Burnout."),
    ("your fizzle", "Your Light Healing spell fizzles!"),
    ("your interrupt", "Your Light Healing spell is interrupted."),
    ("you are stunned", "You are stunned!"),
    ("out of mana", "Insufficient Mana to cast this spell!"),
    ("mez lands", "An orc pawn has been mesmerized."),
    ("mez breaks", "An orc pawn has been awakened by Zary."),
    (
        "damage shield burns you",
        "YOU are burned by Emperor Crush's flames for 6 points of non-melee damage!",
    ),
    (
        "/who listing",
        "[22 PAL/MNK/SHM] Izzin (Kerran) <Ancient Artifacts> ZONE: Najena",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_is_filtered() {
        assert!(is_chat("Bluu tells NewPlayers:1, 'so do we go back'"));
        assert!(is_chat("Soandso says, 'hello'"));
        assert!(!is_chat("You slash an orc pawn for 36 points of damage."));
        assert!(!is_chat("Your Light Healing spell fizzles!"));
    }

    #[test]
    fn templates_collapse_numbers() {
        let dir = tempfile_dir();
        let log = dir.join("eqlog_T_t.txt");
        std::fs::write(
            &log,
            "[Thu Jul 31 10:00:00 2026] You slash a rat for 10 points of damage.\n\
             [Thu Jul 31 10:00:01 2026] You slash a rat for 214 points of damage.\n\
             [Thu Jul 31 10:00:02 2026] Bob says, 'chatter'\n",
        )
        .unwrap();
        let t = unique_templates(&log, 10);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, 2);
        assert!(t[0].1.contains("214"));
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("logscan-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
