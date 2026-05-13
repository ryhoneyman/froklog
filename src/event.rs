use serde::{Deserialize, Serialize};

// ── Hit modifier bitmask flags ────────────────────────────────────────────────
/// Critical hit family (Critical / Deadly Strike / Crippling Blow / Finishing Blow).
pub const MODS_CRIT:          u16 = 0x0001;
pub const MODS_TWINCAST:      u16 = 0x0002;
pub const MODS_LUCKY:         u16 = 0x0004;
/// Rampage or Wild Rampage (AoE melee).
pub const MODS_RAMPAGE:       u16 = 0x0008;
pub const MODS_STRIKETHROUGH: u16 = 0x0010;
/// Riposte used as a hit modifier (Riposte Strikethrough).
pub const MODS_RIPOSTE_MOD:   u16 = 0x0020;
pub const MODS_ASSASSINATE:   u16 = 0x0040;
pub const MODS_HEADSHOT:      u16 = 0x0080;
pub const MODS_SLAY_UNDEAD:   u16 = 0x0100;
pub const MODS_DOUBLEBOW:     u16 = 0x0200;
pub const MODS_FLURRY:        u16 = 0x0400;

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
    Melee { ts: u32, mob: u32, src: String, tgt: String, dmg: u32, typ: String, tank: bool, #[serde(default)] mods: u16 },
    /// Attributed direct-damage spell.  Same `tank` semantics as Melee.
    Spell { ts: u32, mob: u32, src: String, tgt: String, dmg: u32, sp: String, tank: bool, #[serde(default)] mods: u16 },
    /// DoT tick — always player (`src`) → mob (`tgt`).
    Dot   { ts: u32, mob: u32, src: String, tgt: String, dmg: u32, sp: String, #[serde(default)] mods: u16 },
    /// Riposte damage — always player (`src`) riposting mob (`tgt`).
    Rip   { ts: u32, mob: u32, src: String, tgt: String, dmg: u32, #[serde(default)] mods: u16 },
    /// Damage shield proc — always player's (`src`) DS hitting mob (`tgt`).
    Ds    { ts: u32, mob: u32, src: String, tgt: String, dmg: u32 },
    /// Heal.  `mob` is the active mob-instance ID, or `None` when healing outside combat.
    Heal  { ts: u32, mob: Option<u32>, src: String, tgt: String, amt: u32, sp: String, #[serde(default)] mods: u16 },
    /// Mob (`tgt`) confirmed killed by `killer` (empty string if unknown/self).
    Slay  { ts: u32, mob: u32, tgt: String, #[serde(default)] killer: String },
    /// Spell cast started — drives cast-bar display only, not stored long-term.
    Cast  { ts: u32, src: String, sp: String },
    /// Miss/avoidance: `src` attacked `tgt`, `tgt` avoided via `typ`
    /// (dodge / parry / miss / block / riposte / invulnerable / absorb).
    Miss  { ts: u32, mob: u32, src: String, tgt: String, typ: String },
    /// Rune/absorb: `tgt`'s mitigation absorbed a hit from `src` (zero damage).
    Absorb { ts: u32, mob: u32, tgt: String, src: String },
    /// Spell resist: NPC `tgt` resisted caster `src`'s spell `sp`.
    Resist { ts: u32, src: String, tgt: String, sp: String },
    /// Player class detected from a /who log line.
    /// `classes` is 1–3 EQ class short-codes in priority order, e.g. ["WAR","MNK","ROG"].
    Who { ts: u32, name: String, classes: Vec<String> },
}

impl CombatEvent {
    /// The EQ log timestamp (unix seconds) for this event.
    pub fn ts(&self) -> u32 {
        match self {
            Self::Melee { ts, .. } | Self::Spell { ts, .. } | Self::Dot { ts, .. }
            | Self::Rip { ts, .. } | Self::Ds { ts, .. } | Self::Heal { ts, .. }
            | Self::Slay { ts, .. } | Self::Cast { ts, .. }
            | Self::Miss { ts, .. } | Self::Absorb { ts, .. } | Self::Resist { ts, .. }
            | Self::Who { ts, .. } => *ts,
        }
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
