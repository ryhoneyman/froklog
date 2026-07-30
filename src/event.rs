use serde::{Deserialize, Serialize};

// ── Hit modifier bitmask flags ────────────────────────────────────────────────
/// Critical hit family (Critical / Deadly Strike / Crippling Blow / Finishing Blow).
pub const MODS_CRIT: u16 = 0x0001;
pub const MODS_TWINCAST: u16 = 0x0002;
pub const MODS_LUCKY: u16 = 0x0004;
/// Rampage or Wild Rampage (AoE melee).
pub const MODS_RAMPAGE: u16 = 0x0008;
pub const MODS_STRIKETHROUGH: u16 = 0x0010;
/// Riposte used as a hit modifier (Riposte Strikethrough).
pub const MODS_RIPOSTE_MOD: u16 = 0x0020;
pub const MODS_ASSASSINATE: u16 = 0x0040;
pub const MODS_HEADSHOT: u16 = 0x0080;
pub const MODS_SLAY_UNDEAD: u16 = 0x0100;
pub const MODS_DOUBLEBOW: u16 = 0x0200;
pub const MODS_FLURRY: u16 = 0x0400;

fn is_zero(v: &u16) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !v
}

/// A single fully-parsed, attributed combat event emitted by the client.
///
/// `ts`   — unix timestamp in seconds (u32, safe until 2106).
/// `mob`  — per-session sequential mob-instance ID assigned by the client parser.
/// `tank` — on Melee/Spell: `true` = mob attacking player (tanking), `false` = player attacking mob.
/// `mods` — hit modifier bitmask (MODS_* constants); 0 if none or unknown.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum CombatEvent {
    /// Melee hit.  `tank=false`: player→mob (damage).  `tank=true`: mob→player (tanking).
    Melee {
        ts: u32,
        mob: u32,
        src: String,
        tgt: String,
        dmg: u32,
        typ: String,
        #[serde(default, skip_serializing_if = "is_false")]
        tank: bool,
        #[serde(default, skip_serializing_if = "is_zero")]
        mods: u16,
    },
    /// Attributed direct-damage spell.  Same `tank` semantics as Melee.
    Spell {
        ts: u32,
        mob: u32,
        src: String,
        tgt: String,
        dmg: u32,
        sp: String,
        #[serde(default, skip_serializing_if = "is_false")]
        tank: bool,
        #[serde(default, skip_serializing_if = "is_zero")]
        mods: u16,
    },
    /// DoT tick — always player (`src`) → mob (`tgt`).
    Dot {
        ts: u32,
        mob: u32,
        src: String,
        tgt: String,
        dmg: u32,
        sp: String,
        #[serde(default, skip_serializing_if = "is_zero")]
        mods: u16,
    },
    /// Riposte damage — always player (`src`) riposting mob (`tgt`).
    Rip {
        ts: u32,
        mob: u32,
        src: String,
        tgt: String,
        dmg: u32,
        #[serde(default, skip_serializing_if = "is_zero")]
        mods: u16,
    },
    /// Damage shield proc — always player's (`src`) DS hitting mob (`tgt`).
    Ds {
        ts: u32,
        mob: u32,
        src: String,
        tgt: String,
        dmg: u32,
    },
    /// Heal.  `mob` is the active mob-instance ID, or `None` when healing outside combat.
    Heal {
        ts: u32,
        mob: Option<u32>,
        src: String,
        tgt: String,
        amt: u32,
        sp: String,
        #[serde(default, skip_serializing_if = "is_zero")]
        mods: u16,
    },
    /// Mob (`tgt`) confirmed killed by `killer` (empty string if unknown/self).
    Slay {
        ts: u32,
        mob: u32,
        tgt: String,
        #[serde(default)]
        killer: String,
    },
    /// Spell cast started — drives cast-bar display only, not stored long-term.
    Cast { ts: u32, src: String, sp: String },
    /// Miss/avoidance: `src` attacked `tgt`, `tgt` avoided via `typ`
    /// (dodge / parry / miss / block / riposte / invulnerable / absorb).
    Miss {
        ts: u32,
        mob: u32,
        src: String,
        tgt: String,
        typ: String,
    },
    /// Rune/absorb: `tgt`'s mitigation absorbed a hit from `src` (zero damage).
    Absorb {
        ts: u32,
        mob: u32,
        tgt: String,
        src: String,
    },
    /// Spell resist: NPC `tgt` resisted caster `src`'s spell `sp`.
    Resist {
        ts: u32,
        src: String,
        tgt: String,
        sp: String,
    },
    /// Player class detected from a /who log line.
    /// `classes` is 1–3 EQ class short-codes in priority order, e.g. ["WAR","MNK","ROG"].
    Who {
        ts: u32,
        name: String,
        classes: Vec<String>,
        #[serde(default)]
        level: u8,
    },
    /// Player logged in — emitted when "Welcome to EverQuest Legends!" is seen in the log.
    /// Used by the server to cut a new session boundary in the archive.
    Login { ts: u32 },
    /// Currency looted from a corpse.  `mob` = mob-instance ID (0 if unattributed).
    /// Amount is the total in copper (pp×1000 + gp×100 + sp×10 + cp).
    CurrencyLoot { ts: u32, mob: u32, copper: u32 },
    /// Item looted from a corpse and kept in inventory.
    ItemLoot {
        ts: u32,
        mob: u32,
        item: String,
        qty: u32,
    },
    /// Item auto-sold directly from a corpse.  `copper` = sale price (0 = sold for free).
    ItemSell {
        ts: u32,
        mob: u32,
        item: String,
        qty: u32,
        copper: u32,
    },
    /// Item stored in Dragon Hoard from a corpse.
    ItemHoard { ts: u32, mob: u32, item: String },
    /// Item consumed to create an upgraded item (enchant/enhance loot).
    /// `result` is what you are left holding, including its new tier suffix.
    ItemEnhance {
        ts: u32,
        mob: u32,
        item: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    /// Two held items merged at the merchant//merge UI rather than on a corpse.
    /// There is no mob involved, so `mob` is 0.
    ItemMerge { ts: u32, mob: u32, result: String },
    /// Crowd-control state change: `tgt` was parked (mesmerized/enthralled)
    /// or, with `off`, released ("has been awakened by X"). A parked mob is
    /// deliberately idle — viewers keep its encounter open instead of timing
    /// it out, and a mez on an unengaged add registers it as a pull member
    /// before it ever swings.
    Cc {
        ts: u32,
        mob: u32,
        tgt: String,
        #[serde(default, skip_serializing_if = "is_false")]
        off: bool,
    },
    /// Combat heartbeat: a player-state line (stunned, out of mana,
    /// interrupted, life-drained) proving the fight is ongoing while the
    /// player looks idle and no mob is named. Throttled to one per
    /// log-second. Viewers use these to keep encounter windows open.
    #[serde(rename = "hb")]
    Heartbeat { ts: u32 },
}

impl CombatEvent {
    /// The EQ log timestamp (unix seconds) for this event.
    pub fn ts(&self) -> u32 {
        match self {
            Self::Melee { ts, .. }
            | Self::Spell { ts, .. }
            | Self::Dot { ts, .. }
            | Self::Rip { ts, .. }
            | Self::Ds { ts, .. }
            | Self::Heal { ts, .. }
            | Self::Slay { ts, .. }
            | Self::Cast { ts, .. }
            | Self::Miss { ts, .. }
            | Self::Absorb { ts, .. }
            | Self::Resist { ts, .. }
            | Self::Who { ts, .. }
            | Self::Login { ts }
            | Self::CurrencyLoot { ts, .. }
            | Self::ItemLoot { ts, .. }
            | Self::ItemSell { ts, .. }
            | Self::ItemHoard { ts, .. }
            | Self::ItemEnhance { ts, .. }
            | Self::ItemMerge { ts, .. }
            | Self::Cc { ts, .. }
            | Self::Heartbeat { ts } => *ts,
        }
    }
}

#[cfg(test)]
fn melee_event(ts: u32) -> CombatEvent {
    CombatEvent::Melee {
        ts,
        mob: 0,
        src: "A".into(),
        tgt: "B".into(),
        dmg: 10,
        typ: "slash".into(),
        tank: false,
        mods: 0,
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    #[test]
    fn max_log_ts_empty() {
        assert_eq!(
            EventBatch {
                seq: 0,
                events: vec![]
            }
            .max_log_ts(),
            None
        );
    }
    #[test]
    fn max_log_ts_single() {
        assert_eq!(
            EventBatch {
                seq: 0,
                events: vec![melee_event(100)]
            }
            .max_log_ts(),
            Some(100)
        );
    }
    #[test]
    fn max_log_ts_multiple() {
        let batch = EventBatch {
            seq: 1,
            events: vec![melee_event(100), melee_event(300), melee_event(200)],
        };
        assert_eq!(batch.max_log_ts(), Some(300));
    }
}

#[cfg(test)]
mod ts_tests {
    use super::*;

    #[test]
    fn ts_melee() {
        assert_eq!(melee_event(100).ts(), 100);
    }
    #[test]
    fn ts_spell() {
        let ev = CombatEvent::Spell {
            ts: 200,
            mob: 0,
            src: "A".into(),
            tgt: "B".into(),
            dmg: 10,
            sp: "Bolt".into(),
            tank: false,
            mods: 0,
        };
        assert_eq!(ev.ts(), 200);
    }
    #[test]
    fn ts_dot() {
        let ev = CombatEvent::Dot {
            ts: 300,
            mob: 0,
            src: "A".into(),
            tgt: "B".into(),
            dmg: 5,
            sp: "Dot".into(),
            mods: 0,
        };
        assert_eq!(ev.ts(), 300);
    }
    #[test]
    fn ts_rip() {
        let ev = CombatEvent::Rip {
            ts: 400,
            mob: 0,
            src: "A".into(),
            tgt: "B".into(),
            dmg: 50,
            mods: 0,
        };
        assert_eq!(ev.ts(), 400);
    }
    #[test]
    fn ts_ds() {
        let ev = CombatEvent::Ds {
            ts: 500,
            mob: 0,
            src: "A".into(),
            tgt: "B".into(),
            dmg: 20,
        };
        assert_eq!(ev.ts(), 500);
    }
    #[test]
    fn ts_heal() {
        let ev = CombatEvent::Heal {
            ts: 600,
            mob: None,
            src: "A".into(),
            tgt: "B".into(),
            amt: 100,
            sp: "CH".into(),
            mods: 0,
        };
        assert_eq!(ev.ts(), 600);
    }
    #[test]
    fn ts_slay() {
        let ev = CombatEvent::Slay {
            ts: 700,
            mob: 1,
            tgt: "Mob".into(),
            killer: "P".into(),
        };
        assert_eq!(ev.ts(), 700);
    }
    #[test]
    fn ts_cast() {
        let ev = CombatEvent::Cast {
            ts: 800,
            src: "A".into(),
            sp: "Fireball".into(),
        };
        assert_eq!(ev.ts(), 800);
    }
    #[test]
    fn ts_miss() {
        let ev = CombatEvent::Miss {
            ts: 900,
            mob: 0,
            src: "A".into(),
            tgt: "B".into(),
            typ: "dodge".into(),
        };
        assert_eq!(ev.ts(), 900);
    }
    #[test]
    fn ts_absorb() {
        let ev = CombatEvent::Absorb {
            ts: 1000,
            mob: 0,
            tgt: "A".into(),
            src: "B".into(),
        };
        assert_eq!(ev.ts(), 1000);
    }
    #[test]
    fn ts_resist() {
        let ev = CombatEvent::Resist {
            ts: 1100,
            src: "A".into(),
            tgt: "B".into(),
            sp: "Bolt".into(),
        };
        assert_eq!(ev.ts(), 1100);
    }
    #[test]
    fn ts_who() {
        let ev = CombatEvent::Who {
            ts: 1200,
            name: "Rysk".into(),
            classes: vec!["WAR".into()],
            level: 65,
        };
        assert_eq!(ev.ts(), 1200);
    }
}

/// One second's worth of events batched by the Windows client and sent to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventBatch {
    /// Monotonically increasing sequence number (wraps at u32::MAX).
    pub seq: u32,
    pub events: Vec<CombatEvent>,
}

impl EventBatch {
    /// Maximum EQ log timestamp across all events in this batch, or `None` if empty.
    pub fn max_log_ts(&self) -> Option<u64> {
        self.events.iter().map(|e| e.ts() as u64).max()
    }
}
