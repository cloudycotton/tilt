//! Keysym -> keycode resolution over the core keyboard mapping, TigerVNC-style, and the pool of
//! spare keycodes bound on demand for keysyms the layout lacks. Pure, so it unit-tests without X.
//!
//! The core mapping lists a key's group 1 levels 1-2, its group 2 levels 1-2, then its levels
//! above 2 in group 1 and then in group 2: [G1L1, G1L2, G2L1, G2L2, G1L3, G1L4, ...]. A key with
//! one group repeats it in group 2's places. Whose level 3 comes after G2L2 depends on how many
//! levels the key's group 1 has, which the core mapping does not say, so levels 3-4 are only
//! used while no key has a second group. Groups 3-4 have no place at all.

use super::keysym::{
    is_function, is_keypad, keysym_to_char, NO_SYMBOL, XK_CONTROL_L, XK_CONTROL_R, XK_HYPER_R,
    XK_ISO_LEVEL3_SHIFT, XK_META_L, XK_NUM_LOCK, XK_SHIFT_L, XK_SHIFT_R,
};

/// Core state bits (KeyButMask) that are fixed by the protocol.
const SHIFT_MASK: u16 = 1 << 0;
const LOCK_MASK: u16 = 1 << 1;
const CONTROL_MASK: u16 = 1 << 2;
/// XKB reports the effective group in bits 13-14 of a core state.
const GROUP_SHIFT: u16 = 13;
/// GetModifierMapping rows: Shift, Lock, Control, Mod1..Mod5.
const MODIFIER_ROWS: usize = 8;
/// Indices searched within a keycode's keysyms in group 1: levels 1-2, then levels 3-4.
const GROUP1_INDICES: [usize; 4] = [0, 1, 4, 5];
/// In group 2: levels 1-2 only, since its levels 3-4 have no fixed place.
const GROUP2_INDICES: [usize; 2] = [2, 3];

/// A copy of the core keyboard mapping, as returned by GetKeyboardMapping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keymap {
    min_keycode: u8,
    per_keycode: usize,
    keysyms: Vec<u32>,
    /// Some key has a second group, so levels 3-4 have no known place (see the module doc).
    two_groups: bool,
}

/// A set of keycodes, laid out as QueryKeymap reports the keys down: bit k % 8 of byte k / 8.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Keycodes([u8; 32]);

/// The modifiers that decide which symbol of a key the server picks, and which of them the
/// typing session may lift.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModState {
    pub shift: bool,
    pub lock: bool,
    pub num_lock: bool,
    pub level3: bool,
    /// Control, Alt, Meta, Super or Hyper is on: the key is a shortcut, which keeps its Shift.
    pub shortcut: bool,
    /// Every key holding Shift down is the typing session's own press, so it may lift them.
    pub own_shift: bool,
    /// The same for the level-3 shift.
    pub own_level3: bool,
    /// The effective XKB group, 0 (the first) to 3.
    pub group: u8,
}

/// What the modifier mapping says about the modifiers that resolution fakes, lifts or reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    /// A Shift_L/Shift_R key on the Shift modifier, pressed to fake Shift.
    pub shift_key: Option<u8>,
    /// An ISO_Level3_Shift key on some modifier, pressed to fake level 3.
    pub level3_key: Option<u8>,
    /// The keys that can hold Shift down: those on the Shift modifier, and every Shift_L/Shift_R.
    shift_keys: Keycodes,
    /// The keys that can hold level 3: those on its modifier, and every ISO_Level3_Shift.
    level3_keys: Keycodes,
    /// The keys that can hold Control, Alt, Meta, Super or Hyper: those on the modifiers in
    /// `shortcut_mask`, and every key with one of those keysyms first.
    shortcut_keys: Keycodes,
    level3_mask: u16,
    num_lock_mask: u16,
    /// Control and the modifiers with Alt, Meta, Super or Hyper on them.
    shortcut_mask: u16,
}

/// How to type one keysym: its keycode, the modifiers to press around it, and those the session
/// holds itself that are lifted around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPlan {
    pub keycode: u8,
    pub fake_shift: bool,
    pub fake_level3: bool,
    pub release_shift: bool,
    pub release_level3: bool,
}

impl Keymap {
    /// `keysyms` holds `per_keycode` entries for each keycode from `min_keycode` up.
    pub fn new(min_keycode: u8, per_keycode: u8, keysyms: Vec<u32>) -> Keymap {
        let mut keymap = Keymap {
            min_keycode,
            per_keycode: usize::from(per_keycode),
            keysyms,
            two_groups: false,
        };
        keymap.two_groups = keymap.find_two_groups();
        keymap
    }

    /// The keysyms of `keycode`; empty outside the mapping.
    pub fn keysyms(&self, keycode: u8) -> &[u32] {
        self.row(keycode)
            .and_then(|start| self.keysyms.get(start..start + self.per_keycode))
            .unwrap_or(&[])
    }

    /// Records a binding we made: one group of `keysyms`, which the server reports repeated in
    /// group 2's places, then NoSymbol for the rest of the row.
    pub fn set(&mut self, keycode: u8, keysyms: &[u32]) {
        let Some(start) = self.row(keycode) else {
            return;
        };
        if let Some(row) = self.keysyms.get_mut(start..start + self.per_keycode) {
            for (i, slot) in row.iter_mut().enumerate() {
                let level = if GROUP2_INDICES.contains(&i) {
                    i - 2
                } else {
                    i
                };
                *slot = keysyms.get(level).copied().unwrap_or(NO_SYMBOL);
            }
        }
        // The row may have been the last with a second group.
        self.two_groups = self.find_two_groups();
    }

    /// Keycodes whose keysyms are all NoSymbol, lowest first.
    pub fn spare_keycodes(&self) -> Vec<u8> {
        self.keycodes()
            .filter(|&keycode| self.keysyms(keycode).iter().all(|&s| s == NO_SYMBOL))
            .collect()
    }

    /// How to type `keysym` from `state`, TigerVNC-style: a keycode with it at level 1 or 2 of
    /// the active group, else (in group 1 of a keymap with one group) at level 3 or 4,
    /// preferring the fewest modifiers pressed or lifted and then the lowest keycode. None means
    /// the caller binds a spare keycode.
    pub fn plan(&self, keysym: u32, state: ModState, mods: &Modifiers) -> Option<KeyPlan> {
        let indices: &[usize] = match state.group {
            0 if self.two_groups => &GROUP1_INDICES[..2],
            0 => &GROUP1_INDICES,
            1 => &GROUP2_INDICES,
            // Groups 3 and 4 have no place in the core mapping. Function keysyms sit on keys
            // with one group in every layout, which every group wraps to; text takes a spare.
            _ if is_function(keysym) => &GROUP1_INDICES[..2],
            _ => &[],
        };
        self.plan_among(self.keycodes(), indices, keysym, state, mods)
    }

    /// `plan` restricted to `keycodes` whose keys have a single group, as our spare bindings
    /// do: every group wraps to it, so its levels 1-2 type in any group.
    pub fn plan_on(
        &self,
        keycodes: impl Iterator<Item = u8>,
        keysym: u32,
        state: ModState,
        mods: &Modifiers,
    ) -> Option<KeyPlan> {
        self.plan_among(keycodes, &GROUP1_INDICES[..2], keysym, state, mods)
    }

    fn plan_among(
        &self,
        keycodes: impl Iterator<Item = u8>,
        indices: &[usize],
        keysym: u32,
        state: ModState,
        mods: &Modifiers,
    ) -> Option<KeyPlan> {
        if keysym == NO_SYMBOL {
            return None;
        }
        let mut best: Option<(u8, KeyPlan)> = None;
        for keycode in keycodes {
            let syms = self.keysyms(keycode);
            for &index in indices {
                if sym(syms, index) != keysym {
                    continue;
                }
                let Some(plan) = plan_at(syms, keycode, index, state, mods) else {
                    continue;
                };
                let cost = plan.cost();
                if cost == 0 {
                    return Some(plan);
                }
                if best.is_none_or(|(lowest, _)| cost < lowest) {
                    best = Some((cost, plan));
                }
            }
        }
        best.map(|(_, plan)| plan)
    }

    /// Whether some key's group-2 levels 1-2 differ from its group 1's, which a key with one
    /// group repeats there.
    fn find_two_groups(&self) -> bool {
        let group2 = 2..self.per_keycode.min(4);
        self.keycodes().any(|keycode| {
            let syms = self.keysyms(keycode);
            group2.clone().any(|i| sym(syms, i) != sym(syms, i - 2))
        })
    }

    fn keycodes(&self) -> impl Iterator<Item = u8> + '_ {
        let count = self
            .keysyms
            .len()
            .checked_div(self.per_keycode)
            .unwrap_or(0);
        (0..count).map_while(|i| u8::try_from(usize::from(self.min_keycode) + i).ok())
    }

    /// Offset of `keycode`'s row in `keysyms`.
    fn row(&self, keycode: u8) -> Option<usize> {
        let index = keycode.checked_sub(self.min_keycode)?;
        Some(usize::from(index) * self.per_keycode)
    }
}

impl Keycodes {
    pub fn from_bits(bits: [u8; 32]) -> Keycodes {
        Keycodes(bits)
    }

    pub fn insert(&mut self, keycode: u8) {
        self.0[usize::from(keycode / 8)] |= 1 << (keycode % 8);
    }

    pub fn contains(&self, keycode: u8) -> bool {
        self.0[usize::from(keycode / 8)] & 1 << (keycode % 8) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&byte| byte == 0)
    }

    /// The keycodes in both sets.
    pub fn and(&self, other: &Keycodes) -> Keycodes {
        Keycodes(std::array::from_fn(|i| self.0[i] & other.0[i]))
    }

    /// The keycodes in either set.
    pub fn or(&self, other: &Keycodes) -> Keycodes {
        Keycodes(std::array::from_fn(|i| self.0[i] | other.0[i]))
    }

    /// Whether every keycode here is also in `other`.
    pub fn is_subset(&self, other: &Keycodes) -> bool {
        self.0.iter().zip(&other.0).all(|(a, b)| a & !b == 0)
    }

    /// Lowest first.
    pub fn iter(&self) -> impl Iterator<Item = u8> + '_ {
        (0..=u8::MAX).filter(|&keycode| self.contains(keycode))
    }

    /// As 32-bit words, for a CARDINAL property: keycode k is bit k % 32 of word k / 32.
    pub fn to_words(self) -> [u32; 8] {
        std::array::from_fn(|i| u32::from_le_bytes(std::array::from_fn(|j| self.0[4 * i + j])))
    }

    pub fn from_words(words: [u32; 8]) -> Keycodes {
        Keycodes(std::array::from_fn(|i| words[i / 4].to_le_bytes()[i % 4]))
    }
}

impl FromIterator<u8> for Keycodes {
    fn from_iter<I: IntoIterator<Item = u8>>(keycodes: I) -> Keycodes {
        let mut set = Keycodes::default();
        for keycode in keycodes {
            set.insert(keycode);
        }
        set
    }
}

impl Modifiers {
    /// `modmap` is GetModifierMapping's keycodes: 8 rows (Shift, Lock, Control, Mod1..Mod5) of
    /// equal width, 0 in unused slots.
    pub fn new(keymap: &Keymap, modmap: &[u8]) -> Modifiers {
        let mut mods = Modifiers {
            shortcut_mask: CONTROL_MASK,
            ..Modifiers::default()
        };
        // Keys an XKB action makes modifiers of, without a place in the modifier mapping (AltGr
        // is ISO_Level3_Shift on keycode 108 in de, for one).
        for keycode in keymap.keycodes() {
            match keymap.keysyms(keycode).first().copied() {
                Some(XK_SHIFT_L | XK_SHIFT_R) => mods.shift_keys.insert(keycode),
                Some(XK_ISO_LEVEL3_SHIFT) => mods.level3_keys.insert(keycode),
                Some(XK_CONTROL_L | XK_CONTROL_R | XK_META_L..=XK_HYPER_R) => {
                    mods.shortcut_keys.insert(keycode)
                }
                _ => {}
            }
        }
        let width = modmap.len() / MODIFIER_ROWS;
        if width == 0 {
            return mods;
        }
        let rows = || {
            modmap
                .chunks_exact(width)
                .map(|keycodes| keycodes.iter().copied().filter(|&k| k != 0))
                .enumerate()
        };
        for (row, keycodes) in rows() {
            let bit = 1u16 << row;
            for keycode in keycodes {
                let syms = keymap.keysyms(keycode);
                match syms.first().copied() {
                    Some(XK_SHIFT_L | XK_SHIFT_R) if row == 0 => {
                        mods.shift_key.get_or_insert(keycode);
                    }
                    Some(XK_ISO_LEVEL3_SHIFT) => {
                        mods.level3_key.get_or_insert(keycode);
                        mods.level3_mask |= bit;
                    }
                    Some(XK_NUM_LOCK) => mods.num_lock_mask |= bit,
                    _ => {}
                }
                if syms.iter().any(|s| (XK_META_L..=XK_HYPER_R).contains(s)) {
                    mods.shortcut_mask |= bit;
                }
                if row == 0 {
                    mods.shift_keys.insert(keycode);
                }
            }
        }
        // Every key on the level-3 shift's modifier or a shortcut modifier holds it, whatever its
        // keysym.
        for (row, keycodes) in rows() {
            let bit = 1 << row;
            for keycode in keycodes {
                if mods.level3_mask & bit != 0 {
                    mods.level3_keys.insert(keycode);
                }
                if mods.shortcut_mask & bit != 0 {
                    mods.shortcut_keys.insert(keycode);
                }
            }
        }
        mods
    }

    /// The modifiers in a core state mask (e.g. QueryPointer's), with `down` the keys down and
    /// `own` those of them the typing session pressed itself.
    pub fn state(&self, mask: u16, down: &Keycodes, own: &Keycodes) -> ModState {
        // A Shift locked or latched with no key down is no one's to lift.
        let owns = |keys: &Keycodes| {
            let holding = keys.and(down);
            !holding.is_empty() && holding.is_subset(own)
        };
        ModState {
            shift: mask & SHIFT_MASK != 0,
            lock: mask & LOCK_MASK != 0,
            num_lock: mask & self.num_lock_mask != 0,
            level3: mask & self.level3_mask != 0,
            shortcut: mask & self.shortcut_mask != 0,
            own_shift: owns(&self.shift_keys),
            own_level3: owns(&self.level3_keys),
            group: ((mask >> GROUP_SHIFT) & 3) as u8,
        }
    }

    /// The keys among `keys` that can hold Control, Alt, Meta, Super or Hyper.
    pub fn shortcut_keys(&self, keys: &Keycodes) -> Keycodes {
        self.shortcut_keys.and(keys)
    }

    /// The keys down that hold the Shift or level 3 that `plan` lifts.
    pub fn lifted(&self, plan: &KeyPlan, down: &Keycodes) -> Keycodes {
        let none = Keycodes::default();
        let shift = if plan.release_shift {
            &self.shift_keys
        } else {
            &none
        };
        let level3 = if plan.release_level3 {
            &self.level3_keys
        } else {
            &none
        };
        shift.or(level3).and(down)
    }
}

impl KeyPlan {
    /// Modifier changes around the key. A level-3 change costs more than a Shift change, so
    /// levels 1-2 win ties.
    fn cost(&self) -> u8 {
        let shift = u8::from(self.fake_shift) + u8::from(self.release_shift);
        let level3 = u8::from(self.fake_level3) + u8::from(self.release_level3);
        shift + 2 * level3
    }
}

fn sym(syms: &[u32], index: usize) -> u32 {
    syms.get(index).copied().unwrap_or(NO_SYMBOL)
}

/// The plan for typing `syms[index]` on `keycode`, or None when that needs a modifier change we
/// cannot make. Modifiers someone else holds are never lifted: tilt only releases what it
/// pressed.
fn plan_at(
    syms: &[u32],
    keycode: u8,
    index: usize,
    state: ModState,
    mods: &Modifiers,
) -> Option<KeyPlan> {
    let level3 = index >= 4;
    // The pair of levels holding `index`: group 1 levels 1-2, group 2's, or group 1 levels 3-4.
    let base = index & !1;
    // A level-3 shift that is down picks the key's level 3 instead, where it has one. Group 2's
    // levels 3-4 have no fixed place in the core mapping, so any group-2 key may have one; with
    // two groups, those at 4-5 may be group 2's, which errs on the side of lifting too.
    let has_level3 = base == 2 || sym(syms, 4) != NO_SYMBOL || sym(syms, 5) != NO_SYMBOL;
    let release_level3 = !level3 && state.level3 && has_level3;
    if release_level3 && !state.own_level3 {
        return None;
    }
    let fake_level3 = level3 && !state.level3;
    let (lower, upper) = (sym(syms, base), sym(syms, base + 1));
    let (mut fake_shift, mut release_shift) = (false, false);
    if upper != NO_SYMBOL && upper != lower {
        // Caps Lock swaps the levels of letter keys (XKB ALPHABETIC), NumLock those of keypad
        // keys (KEYPAD); Shift then swaps them back.
        let swapped = (state.lock && is_case_pair(lower, upper))
            || (!level3 && state.num_lock && (is_keypad(lower) || is_keypad(upper)));
        let shifted = (index % 2 == 1) != swapped;
        fake_shift = shifted && !state.shift;
        // The session's own Shift is lifted for the unshifted symbol, as TigerVNC does: clients
        // send the character their layout made, Shift and all (German Shift+7 is '/'). Shift
        // stays for shortcuts, and on keys with a function keysym, so Shift+Tab is still
        // ISO_Left_Tab.
        release_shift = !shifted
            && state.shift
            && state.own_shift
            && !state.shortcut
            && !is_function(lower)
            && !is_function(upper);
    }
    if (fake_shift && mods.shift_key.is_none()) || (fake_level3 && mods.level3_key.is_none()) {
        return None;
    }
    Some(KeyPlan {
        keycode,
        fake_shift,
        fake_level3,
        release_shift,
        release_level3,
    })
}

/// Whether `lower`/`upper` are the two cases of one letter, which the server types ALPHABETIC.
/// Legacy non-Latin-1 keysyms are not recognised (their levels then ignore Caps Lock here).
fn is_case_pair(lower: u32, upper: u32) -> bool {
    let (Some(l), Some(u)) = (keysym_to_char(lower), keysym_to_char(upper)) else {
        return false;
    };
    l != u && l.to_uppercase().eq(std::iter::once(u))
}

/// The two keysyms a spare keycode is bound to for `keysym`. A Latin-1 letter gets [lower,
/// upper], as TigerVNC binds it: the server types that row ALPHABETIC, which consumes Caps Lock,
/// so `plan` reaches either case through Shift. On any other row clients upper-case a letter
/// while Caps Lock is on. Everything else, and a letter when there is no Shift key to fake, gets
/// [K, K], which types K at either level. The server knows no case for Unicode keysyms, so a
/// Unicode letter (ж) still comes out upper case under Caps Lock.
pub fn spare_row(keysym: u32, mods: &Modifiers) -> [u32; 2] {
    match latin1_cases(keysym) {
        Some(cases) if mods.shift_key.is_some() => cases,
        _ => [keysym, keysym],
    }
}

/// [lower, upper] for a Latin-1 letter: the Latin-1 pairs of the server's case table, which has
/// none for ß, ÿ and µ.
fn latin1_cases(keysym: u32) -> Option<[u32; 2]> {
    match keysym {
        0x41..=0x5a | 0xc0..=0xd6 | 0xd8..=0xde => Some([keysym + 0x20, keysym]),
        0x61..=0x7a | 0xe0..=0xf6 | 0xf8..=0xfe => Some([keysym, keysym - 0x20]),
        _ => None,
    }
}

/// One of our spare-keycode bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub keycode: u8,
    /// The first keysym of the bound row, which `reconcile` looks for.
    pub keysym: u32,
    used_us: u64,
}

/// A keycode picked for a new binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    pub keycode: u8,
    /// When the binding it replaces was last used; None for a spare that was free.
    pub evicted_used_us: Option<u64>,
}

/// Spare keycodes bound to keysyms on demand, reused least recently used first.
#[derive(Debug, Clone, Default)]
pub struct SpareKeycodes {
    /// Unbound spares we may bind, lowest first. The lowest spare of all is never one of them:
    /// xdotool types a character the layout lacks on the lowest keycode without keysyms (failing
    /// once there is none), and agents type with it.
    free: Vec<u8>,
    /// Our bindings, least recently used first.
    bound: Vec<Binding>,
}

impl SpareKeycodes {
    pub fn new(keymap: &Keymap) -> SpareKeycodes {
        SpareKeycodes {
            free: unreserved(keymap),
            bound: Vec::new(),
        }
    }

    /// Spares available to bind, free or bound by us.
    pub fn capacity(&self) -> usize {
        self.free.len() + self.bound.len()
    }

    pub fn bindings(&self) -> &[Binding] {
        &self.bound
    }

    /// Marks our binding on `keycode`, if any, as used at `now_us`.
    pub fn touch(&mut self, keycode: u8, now_us: u64) {
        if let Some(i) = self.bound.iter().position(|b| b.keycode == keycode) {
            let mut binding = self.bound.remove(i);
            binding.used_us = now_us;
            self.bound.push(binding);
        }
    }

    /// The keycode a new binding takes: the highest free spare that is not `pressed`, else the
    /// least recently used of our bindings whose keycode is not `pressed`. None when every
    /// candidate is pressed. Nothing changes until `bind`.
    pub fn candidate(&self, pressed: impl Fn(u8) -> bool) -> Option<Allocation> {
        if let Some(&keycode) = self.free.iter().rev().find(|&&k| !pressed(k)) {
            return Some(Allocation {
                keycode,
                evicted_used_us: None,
            });
        }
        let old = self.bound.iter().find(|b| !pressed(b.keycode))?;
        Some(Allocation {
            keycode: old.keycode,
            evicted_used_us: Some(old.used_us),
        })
    }

    /// Records `keycode`, a `candidate`, as bound to `keysym` and used at `now_us`.
    pub fn bind(&mut self, keycode: u8, keysym: u32, now_us: u64) {
        self.free.retain(|&k| k != keycode);
        self.bound.retain(|b| b.keycode != keycode);
        self.bound.push(Binding {
            keycode,
            keysym,
            used_us: now_us,
        });
    }

    /// Re-syncs with a freshly read mapping: forgets bindings someone else overwrote (they are no
    /// longer ours to reuse or unbind) and re-reads which spares are free. True if it forgot any.
    pub fn reconcile(&mut self, keymap: &Keymap) -> bool {
        let before = self.bound.len();
        self.bound
            .retain(|b| keymap.keysyms(b.keycode).first() == Some(&b.keysym));
        self.free = unreserved(keymap);
        self.bound.len() != before
    }
}

/// The spare keycodes but the lowest, which is left to agents (see `SpareKeycodes::free`).
fn unreserved(keymap: &Keymap) -> Vec<u8> {
    keymap.spare_keycodes().into_iter().skip(1).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::keysym::{XK_MODE_SWITCH, XK_RETURN, XK_TAB};

    const XK_ISO_LEFT_TAB: u32 = 0xfe20;
    const XK_CAPS_LOCK: u32 = 0xffe5;
    const XK_ALT_L: u32 = 0xffe9;
    const XK_SUPER_L: u32 = 0xffeb;
    const XK_SUPER_R: u32 = 0xffec;
    const XK_KP_HOME: u32 = 0xff95;
    const XK_KP_7: u32 = 0xffb7;
    const XK_EURO: u32 = 0x20ac;
    const XK_BROKENBAR: u32 = 0xa6;
    const XK_DEAD_BELOWMACRON: u32 = 0xfe68;
    const XK_CYRILLIC_EF: u32 = 0x6c6;
    const XK_CYRILLIC_EF_UPPER: u32 = 0x6e6;
    const MOD1: u16 = 1 << 3;
    const MOD2: u16 = 1 << 4;
    const MOD4: u16 = 1 << 6;
    const MOD5: u16 = 1 << 7;

    const PER: u8 = 6;
    const N: u32 = NO_SYMBOL;

    type Row = (u8, [u32; PER as usize]);

    /// A slice of Xvfb's evdev "us" map (keycodes as in `xmodmap -pke`), plus a de-style
    /// EuroSign on level 3 of `e`, and four spare keycodes.
    fn table() -> Vec<Row> {
        vec![
            (10, [0x31, 0x21, 0x31, 0x21, N, N]), // 1 exclam
            (11, [0x32, 0x40, 0x32, 0x40, N, N]), // 2 at
            (21, [0x3d, 0x2b, 0x3d, 0x2b, N, N]), // equal plus
            (23, [XK_TAB, XK_ISO_LEFT_TAB, XK_TAB, XK_ISO_LEFT_TAB, N, N]),
            (26, [0x65, 0x45, 0x65, 0x45, XK_EURO, XK_EURO]), // e E, EuroSign on level 3
            (36, [XK_RETURN, N, XK_RETURN, N, N, N]),
            (37, [XK_CONTROL_L, N, XK_CONTROL_L, N, N, N]),
            (38, [0x61, 0x41, 0x61, 0x41, N, N]), // a A
            (47, [0x3b, 0x3a, 0x3b, 0x3a, N, N]), // semicolon colon
            (48, [0x27, 0x22, 0x27, 0x22, N, N]), // apostrophe quotedbl
            (49, [0x60, 0x7e, 0x60, 0x7e, N, N]), // grave asciitilde
            (50, [XK_SHIFT_L, N, XK_SHIFT_L, N, N, N]),
            (51, [0x5c, 0x7c, 0x5c, 0x7c, N, N]), // backslash bar
            (60, [0x2e, 0x3e, 0x2e, 0x3e, N, N]), // period greater
            (61, [0x2f, 0x3f, 0x2f, 0x3f, N, N]), // slash question
            (62, [XK_SHIFT_R, N, XK_SHIFT_R, N, N, N]),
            (64, [XK_ALT_L, XK_META_L, XK_ALT_L, XK_META_L, N, N]),
            (66, [XK_CAPS_LOCK, N, XK_CAPS_LOCK, N, N, N]),
            (77, [XK_NUM_LOCK, N, XK_NUM_LOCK, N, N, N]),
            (79, [XK_KP_HOME, XK_KP_7, XK_KP_HOME, XK_KP_7, N, N]),
            (92, [XK_ISO_LEVEL3_SHIFT, N, XK_ISO_LEVEL3_SHIFT, N, N, N]),
            (94, [0x3c, 0x3e, 0x3c, 0x3e, 0x7c, XK_BROKENBAR]), // less greater, bar brokenbar
            (133, [XK_SUPER_L, N, XK_SUPER_L, N, N, N]),
            (203, [XK_MODE_SWITCH, N, XK_MODE_SWITCH, N, N, N]),
            (204, [N, XK_ALT_L, N, XK_ALT_L, N, N]),
            (255, [0x1008_ff11, N, 0x1008_ff11, N, N, N]), // XF86AudioLowerVolume
        ]
    }

    /// Keycodes 8..=255 with `rows` filled in; everything else but the spares 100, 120, 200 and
    /// 250 gets a placeholder, so spare lists stay small.
    fn keymap_with(rows: &[Row]) -> Keymap {
        let mut keysyms = vec![NO_SYMBOL; 248 * PER as usize];
        for (keycode, syms) in rows {
            let start = usize::from(keycode - 8) * PER as usize;
            keysyms[start..start + PER as usize].copy_from_slice(syms);
        }
        for keycode in 8u8..=255 {
            let start = usize::from(keycode - 8) * PER as usize;
            let spare = [100, 120, 200, 250].contains(&keycode);
            if !spare && keysyms[start..start + PER as usize].iter().all(|&s| s == N) {
                // Unused vendor keysyms, on keys with one group.
                keysyms[start] = 0x1008_0000 + u32::from(keycode);
                keysyms[start + 2] = keysyms[start];
            }
        }
        Keymap::new(8, PER, keysyms)
    }

    fn keymap() -> Keymap {
        keymap_with(&table())
    }

    /// As Xvfb has it: Shift: Shift_L, Shift_R; Lock: Caps_Lock; Control: Control_L; Mod1:
    /// Alt_L; Mod2: Num_Lock; Mod4: Super_L; Mod5: ISO_Level3_Shift, Mode_switch.
    fn modmap() -> Vec<u8> {
        vec![50, 62, 66, 0, 37, 0, 64, 0, 77, 0, 0, 0, 133, 0, 92, 203]
    }

    fn mods() -> Modifiers {
        Modifiers::new(&keymap(), &modmap())
    }

    fn plan(keysym: u32, state: ModState) -> Option<KeyPlan> {
        keymap().plan(keysym, state, &mods())
    }

    fn key(keycode: u8, fake_shift: bool, fake_level3: bool) -> Option<KeyPlan> {
        Some(KeyPlan {
            keycode,
            fake_shift,
            fake_level3,
            release_shift: false,
            release_level3: false,
        })
    }

    /// `keycode` with the session's own Shift (`shift`) and level-3 shift (`level3`) lifted.
    fn lifting(keycode: u8, shift: bool, level3: bool, fake_level3: bool) -> Option<KeyPlan> {
        Some(KeyPlan {
            keycode,
            fake_shift: false,
            fake_level3,
            release_shift: shift,
            release_level3: level3,
        })
    }

    fn keycodes(list: &[u8]) -> Keycodes {
        list.iter().copied().collect()
    }

    const NONE: ModState = ModState {
        shift: false,
        lock: false,
        num_lock: false,
        level3: false,
        shortcut: false,
        own_shift: false,
        own_level3: false,
        group: 0,
    };
    /// Shift held by someone else: an agent.
    const SHIFT: ModState = ModState {
        shift: true,
        ..NONE
    };
    /// Shift held by the typing session itself.
    const OWN_SHIFT: ModState = ModState {
        own_shift: true,
        ..SHIFT
    };
    const LOCK: ModState = ModState { lock: true, ..NONE };
    const LEVEL3: ModState = ModState {
        level3: true,
        ..NONE
    };
    const OWN_LEVEL3: ModState = ModState {
        own_level3: true,
        ..LEVEL3
    };
    const NUM_LOCK: ModState = ModState {
        num_lock: true,
        ..NONE
    };
    const GROUP2: ModState = ModState { group: 1, ..NONE };

    #[test]
    fn keysyms_rows_and_bounds() {
        let km = keymap();
        assert_eq!(km.keysyms(38), &[0x61, 0x41, 0x61, 0x41, N, N]);
        assert_eq!(km.keysyms(255)[0], 0x1008_ff11);
        assert!(km.keysyms(7).is_empty(), "below min_keycode");
        assert!(Keymap::new(8, 0, vec![]).keysyms(8).is_empty());
        assert!(
            Keymap::new(8, 2, vec![1, 2]).keysyms(9).is_empty(),
            "past the end"
        );
    }

    #[test]
    fn spare_keycodes_are_the_all_nosymbol_rows_lowest_first() {
        assert_eq!(keymap().spare_keycodes(), vec![100, 120, 200, 250]);
    }

    #[test]
    fn set_writes_one_group_repeated_in_group_two_and_pads_it() {
        let mut km = keymap();
        km.set(250, &[0xfc, 0xdc]);
        assert_eq!(km.keysyms(250), &[0xfc, 0xdc, 0xfc, 0xdc, N, N]);
        assert_eq!(km.spare_keycodes(), vec![100, 120, 200]);
        km.set(250, &[N]);
        assert_eq!(km.spare_keycodes(), vec![100, 120, 200, 250]);
        km.set(3, &[1]); // below the range: ignored
        assert_eq!(km, {
            let mut k = keymap();
            k.set(250, &[N]);
            k
        });
        // Rows too short for group 2 keep what fits.
        let mut short = Keymap::new(8, 1, vec![N; 4]);
        short.set(9, &[0xfc, 0xdc]);
        assert_eq!(short.keysyms(9), &[0xfc]);
    }

    #[test]
    fn keycode_sets() {
        let mut set = keycodes(&[0, 9, 50, 255]);
        assert!(set.contains(0) && set.contains(9) && set.contains(50) && set.contains(255));
        assert!(!set.contains(8) && !set.contains(51));
        assert_eq!(set.iter().collect::<Vec<_>>(), [0, 9, 50, 255]);
        set.insert(51);
        assert_eq!(set.and(&keycodes(&[50, 51, 52])), keycodes(&[50, 51]));
        assert_eq!(keycodes(&[1]).or(&keycodes(&[200])), keycodes(&[1, 200]));
        assert!(keycodes(&[50]).is_subset(&set) && !keycodes(&[50, 52]).is_subset(&set));
        assert!(Keycodes::default().is_empty() && !set.is_empty());
        // QueryKeymap's layout: keycode 50 is bit 2 of byte 6.
        let mut bits = [0; 32];
        bits[6] = 1 << 2;
        assert_eq!(Keycodes::from_bits(bits), keycodes(&[50]));
        let words = keycodes(&[0, 33, 50, 255]).to_words();
        assert_eq!(words, [1, 1 << 1 | 1 << 18, 0, 0, 0, 0, 0, 1 << 31]);
        assert_eq!(Keycodes::from_words(words), keycodes(&[0, 33, 50, 255]));
    }

    #[test]
    fn modifiers_come_from_the_modifier_mapping() {
        let m = mods();
        let none = Keycodes::default();
        assert_eq!(m.shift_key, Some(50));
        assert_eq!(m.level3_key, Some(92));
        assert_eq!(m.level3_mask, MOD5);
        assert_eq!(m.num_lock_mask, MOD2);
        assert_eq!(m.shortcut_mask, CONTROL_MASK | MOD1 | MOD4);
        assert_eq!(m.shift_keys, keycodes(&[50, 62]));
        assert_eq!(m.level3_keys, keycodes(&[92, 203]), "all of Mod5");
        assert_eq!(m.shortcut_keys, keycodes(&[37, 64, 133]));
        assert_eq!(m.state(0, &none, &none), NONE);
        assert_eq!(
            m.state(SHIFT_MASK | LOCK_MASK, &none, &none),
            ModState {
                lock: true,
                ..SHIFT
            }
        );
        assert_eq!(
            m.state(MOD2 | MOD5, &none, &none),
            ModState {
                num_lock: true,
                ..LEVEL3
            }
        );
        // Control and the modifiers with Alt, Meta, Super or Hyper make shortcuts.
        for mask in [CONTROL_MASK, MOD1, MOD4] {
            assert_eq!(
                m.state(mask, &none, &none),
                ModState {
                    shortcut: true,
                    ..NONE
                }
            );
        }
        // The group is in bits 13-14.
        assert_eq!(m.state(1 << 13, &none, &none), GROUP2);
        assert_eq!(m.state(3 << 13, &none, &none).group, 3);
    }

    #[test]
    fn modifier_keys_outside_the_modifier_mapping_count() {
        // de's AltGr: ISO_Level3_Shift on 108, which only its XKB action makes a modifier.
        let km = keymap_with(&[
            (50, [XK_SHIFT_L, N, XK_SHIFT_L, N, N, N]),
            (92, [XK_ISO_LEVEL3_SHIFT, N, XK_ISO_LEVEL3_SHIFT, N, N, N]),
            (108, [XK_ISO_LEVEL3_SHIFT, N, XK_ISO_LEVEL3_SHIFT, N, N, N]),
        ]);
        let m = Modifiers::new(&km, &[50, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 92, 0]);
        assert_eq!(m.level3_keys, keycodes(&[92, 108]));
        // So an agent holding 108 leaves the session's 92 alone.
        let state = m.state(MOD5, &keycodes(&[92, 108]), &keycodes(&[92]));
        assert!(state.level3 && !state.own_level3);
    }

    #[test]
    fn shortcut_keys_are_every_key_on_a_shortcut_modifier_or_with_its_keysym() {
        // Caps Lock added to Control (`xmodmap -e 'add control = Caps_Lock'`), and Control_R and
        // Super_R outside the modifier mapping.
        let km = keymap_with(&[
            (37, [XK_CONTROL_L, N, XK_CONTROL_L, N, N, N]),
            (50, [XK_SHIFT_L, N, XK_SHIFT_L, N, N, N]),
            (64, [XK_ALT_L, XK_META_L, XK_ALT_L, XK_META_L, N, N]),
            (66, [XK_CAPS_LOCK, N, XK_CAPS_LOCK, N, N, N]),
            (92, [XK_ISO_LEVEL3_SHIFT, N, XK_ISO_LEVEL3_SHIFT, N, N, N]),
            (105, [XK_CONTROL_R, N, XK_CONTROL_R, N, N, N]),
            (134, [XK_SUPER_R, N, XK_SUPER_R, N, N, N]),
        ]);
        let m = Modifiers::new(&km, &[50, 0, 0, 0, 37, 66, 64, 0, 0, 0, 0, 0, 0, 0, 92, 0]);
        let all = keycodes(&[37, 50, 64, 66, 92, 105, 134]);
        assert_eq!(m.shortcut_keys(&all), keycodes(&[37, 64, 66, 105, 134]));
        assert_eq!(m.shortcut_keys(&keycodes(&[50, 64])), keycodes(&[64]));
    }

    #[test]
    fn a_modifier_is_the_sessions_own_when_it_holds_every_key_holding_it() {
        let m = mods();
        let down_shift = |down: &[u8], own: &[u8]| {
            m.state(SHIFT_MASK, &keycodes(down), &keycodes(own))
                .own_shift
        };
        assert!(down_shift(&[50], &[50]));
        assert!(
            down_shift(&[50, 62, 38], &[50, 62]),
            "other keys do not matter"
        );
        assert!(!down_shift(&[50], &[]), "an agent's Shift");
        assert!(!down_shift(&[50, 62], &[50]), "an agent holds Shift_R too");
        assert!(!down_shift(&[], &[50]), "a locked or latched Shift");
        let state = m.state(MOD5, &keycodes(&[92]), &keycodes(&[92]));
        assert!(state.own_level3 && !state.own_shift);
    }

    #[test]
    fn modifiers_tolerate_empty_or_partial_maps() {
        let empty = Modifiers::new(&keymap(), &[]);
        assert_eq!((empty.shift_key, empty.level3_key), (None, None));
        assert_eq!(empty.level3_mask | empty.num_lock_mask, 0);
        assert_eq!(
            empty.shortcut_mask, CONTROL_MASK,
            "Control is fixed by the protocol"
        );
        // No ISO_Level3_Shift on any modifier, Shift row holds only Caps_Lock.
        let m = Modifiers::new(&keymap(), &[66, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(m.shift_key, None);
        assert_eq!(m.level3_key, None);
        let none = Keycodes::default();
        assert_eq!(m.state(MOD5, &none, &none), NONE);
    }

    #[test]
    fn level_one_keysyms_need_no_modifier() {
        assert_eq!(plan(0x61, NONE), key(38, false, false)); // a
        assert_eq!(plan(0x31, NONE), key(10, false, false)); // 1
        assert_eq!(plan(XK_RETURN, NONE), key(36, false, false));
        assert_eq!(plan(XK_SHIFT_L, NONE), key(50, false, false));
        assert_eq!(plan(0x1008_ff11, NONE), key(255, false, false));
    }

    #[test]
    fn level_two_keysyms_fake_shift_unless_it_is_held() {
        assert_eq!(plan(0x41, NONE), key(38, true, false)); // A
        assert_eq!(plan(0x21, NONE), key(10, true, false)); // exclam
        for state in [SHIFT, OWN_SHIFT] {
            assert_eq!(plan(0x41, state), key(38, false, false));
            assert_eq!(plan(0x21, state), key(10, false, false));
        }
    }

    #[test]
    fn the_sessions_own_shift_is_lifted_for_unshifted_characters() {
        // What German and French clients send with Shift down, typed on the server's US keys,
        // and a client with Caps Lock on sending Shift+a.
        for (keysym, keycode) in [
            (0x2f, 61), // de Shift+7: slash
            (0x3d, 21), // de Shift+0: equal
            (0x3b, 47), // de Shift+,: semicolon
            (0x27, 48), // de Shift+#: apostrophe
            (0x60, 49), // de Shift+´: grave
            (0x31, 10), // fr Shift+&: 1
            (0x2e, 60), // fr Shift+;: period
            (0x61, 38), // a
        ] {
            assert_eq!(
                plan(keysym, OWN_SHIFT),
                lifting(keycode, true, false, false),
                "{keysym:#x}"
            );
        }
        // Shifted characters keep the Shift.
        assert_eq!(plan(0x3f, OWN_SHIFT), key(61, false, false)); // question
        assert_eq!(plan(0x40, OWN_SHIFT), key(11, false, false)); // at

        // With Caps Lock on at the server, A is the unshifted symbol of a.
        let lock = ModState {
            lock: true,
            ..OWN_SHIFT
        };
        assert_eq!(plan(0x41, lock), lifting(38, true, false, false));
        assert_eq!(plan(0x61, lock), key(38, false, false));
    }

    #[test]
    fn a_held_shift_stays_for_shortcuts_non_characters_and_agents() {
        // Shift+Tab and Shift+Return: the Shift applies, as the user intends.
        assert_eq!(plan(XK_TAB, OWN_SHIFT), key(23, false, false));
        assert_eq!(plan(XK_RETURN, OWN_SHIFT), key(36, false, false));
        // Keypad keys' other level is no character either.
        assert_eq!(plan(XK_KP_HOME, OWN_SHIFT), key(79, false, false));
        // Control+Shift+/ stays a shortcut with Shift.
        let shortcut = ModState {
            shortcut: true,
            ..OWN_SHIFT
        };
        assert_eq!(plan(0x2f, shortcut), key(61, false, false));
        // Someone else's Shift is never released: '/' comes out as '?'.
        assert_eq!(plan(0x2f, SHIFT), key(61, false, false));
        assert_eq!(plan(0x61, SHIFT), key(38, false, false));
    }

    #[test]
    fn caps_lock_swaps_letter_levels_only() {
        assert_eq!(plan(0x41, LOCK), key(38, false, false)); // A: Lock already gives it
        assert_eq!(plan(0x61, LOCK), key(38, true, false)); // a: Shift cancels Lock
        assert_eq!(plan(0x21, LOCK), key(10, true, false)); // exclam: TWO_LEVEL ignores Lock
        assert_eq!(plan(0x31, LOCK), key(10, false, false));
    }

    #[test]
    fn num_lock_swaps_keypad_levels() {
        assert_eq!(plan(XK_KP_7, NUM_LOCK), key(79, false, false));
        assert_eq!(plan(XK_KP_7, NONE), key(79, true, false));
        assert_eq!(plan(XK_KP_HOME, NUM_LOCK), key(79, true, false));
        assert_eq!(plan(XK_KP_HOME, NONE), key(79, false, false));
    }

    #[test]
    fn level_three_fakes_iso_level3_shift() {
        assert_eq!(plan(XK_EURO, NONE), key(26, false, true)); // EuroSign EuroSign: no Shift
        assert_eq!(plan(XK_BROKENBAR, NONE), key(94, true, true));
        assert_eq!(plan(XK_BROKENBAR, SHIFT), key(94, false, true));
        assert_eq!(plan(XK_EURO, LEVEL3), key(26, false, false));
        assert_eq!(
            plan(
                XK_BROKENBAR,
                ModState {
                    shift: true,
                    ..LEVEL3
                }
            ),
            key(94, false, false)
        );
    }

    #[test]
    fn fewer_modifier_changes_win() {
        // bar is level 2 of backslash (fake Shift) and level 3 of less (fake level 3).
        assert_eq!(plan(0x7c, NONE), key(51, true, false));
        // With level 3 held, 94's level 3 needs nothing faked, which beats 51's fake Shift.
        assert_eq!(plan(0x7c, LEVEL3), key(94, false, false));
        // Alt_L is level 1 of 64 and level 2 of 204: 64 needs no fake Shift.
        assert_eq!(plan(XK_ALT_L, NONE), key(64, false, false));
        assert_eq!(plan(XK_ALT_L, SHIFT), key(64, false, false));
        // With the session's Shift held, bar is level 2 of 51 as it stands, which beats lifting
        // Shift and faking level 3 on 94.
        assert_eq!(plan(0x7c, OWN_SHIFT), key(51, false, false));
    }

    #[test]
    fn a_held_level3_is_lifted_if_the_session_holds_it_and_disqualifies_otherwise() {
        assert_eq!(plan(0x65, LEVEL3), None); // e would come out as EuroSign
        assert_eq!(plan(0x65, OWN_LEVEL3), lifting(26, false, true, false));
        // a has no level 3, so the level-3 shift does not matter.
        assert_eq!(plan(0x61, LEVEL3), key(38, false, false));
        assert_eq!(plan(0x61, OWN_LEVEL3), key(38, false, false));
    }

    /// Rows of Xvfb's evdev map after `setxkbmap de`.
    fn de() -> (Keymap, Modifiers) {
        let km = keymap_with(&[
            (10, [0x31, 0x21, 0x31, 0x21, 0xb9, 0xa1]), // 1 exclam onesuperior exclamdown
            (16, [0x37, 0x2f, 0x37, 0x2f, 0x7b, 0xac6]), // 7 slash braceleft seveneighths
            (24, [0x71, 0x51, 0x71, 0x51, 0x40, 0x7d9]), // q Q at Greek_OMEGA
            (35, [0x2b, 0x2a, 0x2b, 0x2a, 0x7e, 0xaf]), // plus asterisk asciitilde macron
            (37, [XK_CONTROL_L, N, XK_CONTROL_L, N, N, N]),
            (50, [XK_SHIFT_L, N, XK_SHIFT_L, N, N, N]),
            (62, [XK_SHIFT_R, N, XK_SHIFT_R, N, N, N]),
            (92, [XK_ISO_LEVEL3_SHIFT, N, XK_ISO_LEVEL3_SHIFT, N, N, N]),
            (94, [0x3c, 0x3e, 0x3c, 0x3e, 0x7c, XK_DEAD_BELOWMACRON]), // less greater bar
            (108, [XK_ISO_LEVEL3_SHIFT, N, XK_ISO_LEVEL3_SHIFT, N, N, N]),
        ]);
        let m = Modifiers::new(&km, &[50, 62, 0, 0, 37, 0, 0, 0, 0, 0, 0, 0, 0, 0, 92, 0]);
        (km, m)
    }

    #[test]
    fn on_a_german_server_lifting_combines_with_faking() {
        let (km, m) = de();
        // A US client's Shift+[ is braceleft: AltGr+7 here, so the session's Shift is lifted
        // and level 3 faked; level 4 would be seveneighths.
        assert_eq!(km.plan(0x7b, OWN_SHIFT, &m), lifting(16, true, false, true));
        assert_eq!(km.plan(0x7b, SHIFT, &m), key(16, false, true));
        // A US client's Shift+= and Shift+\ are '+' and '|': unshifted here, and level 3 beside a
        // dead key, which comes out unless Shift is lifted.
        assert_eq!(
            km.plan(0x2b, OWN_SHIFT, &m),
            lifting(35, true, false, false)
        );
        assert_eq!(km.plan(0x7c, OWN_SHIFT, &m), lifting(94, true, false, true));
        // The session's AltGr is lifted for q, which would otherwise come out as at.
        assert_eq!(
            km.plan(0x71, OWN_LEVEL3, &m),
            lifting(24, false, true, false)
        );
        assert_eq!(km.plan(0x71, LEVEL3, &m), None);
        assert_eq!(km.plan(0x40, OWN_LEVEL3, &m), key(24, false, false));
        // Both lifted for 1.
        let both = ModState {
            own_level3: true,
            level3: true,
            ..OWN_SHIFT
        };
        assert_eq!(km.plan(0x31, both, &m), lifting(10, true, true, false));
    }

    /// Rows of Xvfb's evdev map after `setxkbmap -layout us,ru`.
    fn us_ru() -> Keymap {
        keymap_with(&[
            (10, [0x31, 0x21, 0x31, 0x21, N, N]), // 1 exclam, in both groups
            (38, [0x61, 0x41, XK_CYRILLIC_EF, XK_CYRILLIC_EF_UPPER, N, N]),
            (50, [XK_SHIFT_L, N, XK_SHIFT_L, N, N, N]),
            (94, [0x3c, 0x3e, 0x2f, 0x7c, 0x7c, XK_BROKENBAR]), // less greater, slash bar
        ])
    }

    #[test]
    fn the_active_group_picks_the_columns() {
        let km = us_ru();
        let m = Modifiers::new(&km, &[50, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(km.plan(0x61, NONE, &m), key(38, false, false));
        assert_eq!(km.plan(XK_CYRILLIC_EF, NONE, &m), None);
        // Group 2 types Cyrillic on 38, so a takes a spare keycode.
        assert_eq!(km.plan(0x61, GROUP2, &m), None);
        assert_eq!(km.plan(XK_CYRILLIC_EF, GROUP2, &m), key(38, false, false));
        assert_eq!(
            km.plan(XK_CYRILLIC_EF_UPPER, GROUP2, &m),
            key(38, true, false)
        );
        assert_eq!(km.plan(0x21, GROUP2, &m), key(10, true, false));
        assert_eq!(km.plan(0x2f, GROUP2, &m), key(94, false, false));
        // Group 1's level 3 is not group 2's, and with two groups not known to be group 1's
        // either (see with_two_groups_levels_three_and_four_take_a_spare).
        assert_eq!(km.plan(XK_BROKENBAR, GROUP2, &m), None);
        assert_eq!(km.plan(XK_BROKENBAR, NONE, &m), None);
        // A level-3 shift held in group 2 may pick a level 3 that the core mapping cannot show.
        let held = ModState {
            level3: true,
            ..GROUP2
        };
        assert_eq!(km.plan(0x31, held, &m), None);
        // Groups 3 and 4 have no columns: text takes a spare, and function keysyms the keys
        // with one group that every group wraps to.
        for group in [2, 3] {
            let state = ModState { group, ..NONE };
            assert_eq!(km.plan(0x31, state, &m), None);
            assert_eq!(km.plan(XK_SHIFT_L, state, &m), key(50, false, false));
        }
    }

    /// Rows of Xvfb's evdev map after `setxkbmap -layout us,de`. us has two levels on e and de
    /// four, so EuroSign sits where group 1's level 3 would; y and z swap places in de.
    fn us_de() -> (Keymap, Modifiers) {
        let km = keymap_with(&[
            (26, [0x65, 0x45, 0x65, 0x45, XK_EURO, XK_EURO]),
            (29, [0x79, 0x59, 0x7a, 0x5a, N, N]), // y Y, z Z
            (50, [XK_SHIFT_L, N, XK_SHIFT_L, N, N, N]),
            (52, [0x7a, 0x5a, 0x79, 0x59, N, N]), // z Z, y Y
            (92, [XK_ISO_LEVEL3_SHIFT, N, XK_ISO_LEVEL3_SHIFT, N, N, N]),
        ]);
        let m = Modifiers::new(&km, &[50, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 92, 0]);
        (km, m)
    }

    #[test]
    fn with_two_groups_levels_three_and_four_take_a_spare() {
        let (km, m) = us_de();
        // In us, AltGr+e is e: EuroSign is de's level 3.
        assert_eq!(km.plan(XK_EURO, NONE, &m), None);
        assert_eq!(km.plan(XK_EURO, GROUP2, &m), None);
        assert_eq!(km.plan(0x79, NONE, &m), key(29, false, false));
        assert_eq!(km.plan(0x79, GROUP2, &m), key(52, false, false));
        // With one group, the same row's EuroSign is level 3.
        let mut one_group = km.clone();
        for keycode in [29, 52] {
            one_group.set(keycode, &[NO_SYMBOL]);
        }
        assert_eq!(one_group.plan(XK_EURO, NONE, &m), key(26, false, true));
        assert_eq!(keymap().plan(XK_EURO, NONE, &mods()), key(26, false, true));
    }

    #[test]
    fn single_group_keys_type_in_every_group() {
        let mut km = us_ru();
        let m = Modifiers::new(&km, &[50, 0, 0, 0, 0, 0, 0, 0]);
        km.set(250, &spare_row(0x61, &m));
        for group in 0..4 {
            let state = ModState { group, ..NONE };
            let on_spares = |keysym| km.plan_on([250].into_iter(), keysym, state, &m);
            assert_eq!(on_spares(0x61), key(250, false, false), "group {group}");
            assert_eq!(on_spares(0x41), key(250, true, false), "group {group}");
            assert_eq!(on_spares(0x62), None);
        }
        // The binding also shows in group 2's columns, as the server reports it.
        assert_eq!(km.plan(0x61, GROUP2, &m), key(250, false, false));
    }

    #[test]
    fn missing_keysyms_need_a_spare() {
        assert_eq!(plan(0xfc, NONE), None); // udiaeresis
        assert_eq!(plan(0x0100_4f60, NONE), None); // 你
        assert_eq!(plan(NO_SYMBOL, NONE), None);
    }

    #[test]
    fn unfakeable_modifiers_need_a_spare() {
        let km = keymap();
        let no_shift = Modifiers::new(&km, &[0, 0, 66, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 92, 0]);
        assert_eq!(km.plan(0x41, NONE, &no_shift), None);
        assert_eq!(km.plan(0x41, SHIFT, &no_shift), key(38, false, false));
        assert_eq!(km.plan(XK_EURO, NONE, &no_shift), key(26, false, true));
        let no_level3 = Modifiers::new(&km, &[50, 0, 66, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(km.plan(XK_BROKENBAR, NONE, &no_level3), None);
        assert_eq!(km.plan(0x41, NONE, &no_level3), key(38, true, false));
    }

    #[test]
    fn lifted_keys_are_the_ones_down() {
        let m = mods();
        let plan = |release_shift, release_level3| KeyPlan {
            keycode: 61,
            fake_shift: false,
            fake_level3: false,
            release_shift,
            release_level3,
        };
        let down = keycodes(&[50, 61, 92]);
        assert_eq!(m.lifted(&plan(true, false), &down), keycodes(&[50]));
        assert_eq!(m.lifted(&plan(false, true), &down), keycodes(&[92]));
        assert_eq!(m.lifted(&plan(true, true), &down), keycodes(&[50, 92]));
        assert!(m.lifted(&plan(false, false), &down).is_empty());
    }

    #[test]
    fn spare_rows_pair_latin1_letters_with_their_other_case() {
        let m = mods();
        assert_eq!(spare_row(0xfc, &m), [0xfc, 0xdc]); // udiaeresis Udiaeresis
        assert_eq!(spare_row(0xdc, &m), [0xfc, 0xdc]);
        assert_eq!(spare_row(0x71, &m), [0x71, 0x51]); // q Q
        assert_eq!(spare_row(0xc0, &m), [0xe0, 0xc0]); // Agrave
        assert_eq!(spare_row(0xd8, &m), [0xf8, 0xd8]); // Oslash
        assert_eq!(spare_row(0xfe, &m), [0xfe, 0xde]); // thorn THORN

        // ssharp, ydiaeresis and mu have no Latin-1 upper case; multiply and division are not
        // letters; EuroSign, Unicode € and ж, and legacy Cyrillic_a are not Latin-1.
        for keysym in [
            0xdf,
            0xff,
            0xb5,
            0xd7,
            0xf7,
            0x31,
            XK_EURO,
            0x0100_20ac,
            0x0100_0436,
            0x6c1,
        ] {
            assert_eq!(spare_row(keysym, &m), [keysym, keysym], "{keysym:#x}");
        }
        // With no Shift key to fake, only level 1 is reachable.
        assert_eq!(spare_row(0xdc, &Modifiers::default()), [0xdc, 0xdc]);
    }

    #[test]
    fn a_bound_spare_is_planned_like_a_layout_key() {
        let mut km = keymap();
        km.set(250, &spare_row(0xfc, &mods()));
        // [ü, Ü] is ALPHABETIC: Shift picks the case and Caps Lock swaps it.
        assert_eq!(km.plan(0xfc, NONE, &mods()), key(250, false, false));
        assert_eq!(km.plan(0xdc, NONE, &mods()), key(250, true, false));
        assert_eq!(km.plan(0xfc, LOCK, &mods()), key(250, true, false));
        assert_eq!(km.plan(0xdc, LOCK, &mods()), key(250, false, false));
        assert_eq!(
            km.plan(0xfc, OWN_SHIFT, &mods()),
            lifting(250, true, false, false)
        );
        // [K, K]: the same keysym on both levels, so neither Shift nor Caps Lock matters.
        km.set(200, &spare_row(0x0100_4f60, &mods()));
        for state in [NONE, SHIFT, OWN_SHIFT, LOCK, LEVEL3] {
            assert_eq!(
                km.plan(0x0100_4f60, state, &mods()),
                key(200, false, false),
                "{state:?}"
            );
        }
    }

    #[test]
    fn case_pairs() {
        assert!(is_case_pair(0x61, 0x41)); // a A
        assert!(is_case_pair(0xe4, 0xc4)); // adiaeresis Adiaeresis
        assert!(is_case_pair(0x0100_0436, 0x0100_0416)); // Unicode ж Ж
        assert!(!is_case_pair(0x31, 0x21)); // 1 !
        assert!(!is_case_pair(0x41, 0x61)); // reversed
        assert!(!is_case_pair(0x61, 0x61));
        assert!(!is_case_pair(0xdf, 0xdf)); // ssharp has no single-char uppercase
        assert!(!is_case_pair(0x6c1, 0x6e1)); // legacy Cyrillic keysyms: not recognised
    }

    /// `candidate` then `bind`, as the input thread allocates.
    fn allocate(
        pool: &mut SpareKeycodes,
        keysym: u32,
        now_us: u64,
        pressed: impl Fn(u8) -> bool,
    ) -> Option<Allocation> {
        let allocation = pool.candidate(pressed)?;
        pool.bind(allocation.keycode, keysym, now_us);
        Some(allocation)
    }

    #[test]
    fn spares_allocate_from_the_top_and_leave_the_lowest_to_agents() {
        let mut pool = SpareKeycodes::new(&keymap());
        assert_eq!(pool.capacity(), 3, "100 is xdotool's");
        let picks: Vec<u8> = (0..5)
            .map(|i| {
                allocate(&mut pool, 0x100 + i, u64::from(i), |_| false)
                    .unwrap()
                    .keycode
            })
            .collect();
        // Then the least recently used binding, and never 100.
        assert_eq!(picks, vec![250, 200, 120, 250, 200]);
        assert_eq!(pool.capacity(), 3);
        assert_eq!(
            pool.bindings()
                .iter()
                .map(|b| (b.keycode, b.keysym))
                .collect::<Vec<_>>(),
            vec![(120, 0x102), (250, 0x103), (200, 0x104)]
        );
    }

    #[test]
    fn a_candidate_changes_nothing_until_bound() {
        let mut pool = SpareKeycodes::new(&keymap());
        let first = pool.candidate(|_| false);
        assert_eq!(pool.candidate(|_| false), first);
        assert_eq!(
            first,
            Some(Allocation {
                keycode: 250,
                evicted_used_us: None
            })
        );
        pool.bind(250, 0xa1, 7);
        assert_eq!(pool.candidate(|_| false).unwrap().keycode, 200);
        assert_eq!(pool.bindings().len(), 1);
    }

    #[test]
    fn lru_evicts_the_least_recently_used() {
        let mut pool = SpareKeycodes::new(&keymap());
        for (i, keysym) in [0xa1, 0xa2, 0xa3].into_iter().enumerate() {
            allocate(&mut pool, keysym, i as u64 * 10, |_| false).unwrap();
        }
        // 250 (0xa1, used at 0) is oldest; touching it makes 200 (0xa2, used at 10) the victim.
        pool.touch(250, 100);
        assert_eq!(
            allocate(&mut pool, 0xb1, 200, |_| false),
            Some(Allocation {
                keycode: 200,
                evicted_used_us: Some(10)
            })
        );
        // Touching a keycode that is not ours changes nothing.
        pool.touch(38, 300);
        assert_eq!(
            allocate(&mut pool, 0xb2, 400, |_| false).unwrap().keycode,
            120
        );
    }

    #[test]
    fn a_pressed_keycode_is_never_allocated() {
        let mut pool = SpareKeycodes::new(&keymap());
        // A free spare that is down (held through a binding that was wiped) is skipped too.
        assert_eq!(
            allocate(&mut pool, 0xa1, 0, |k| k == 250).unwrap().keycode,
            200
        );
        allocate(&mut pool, 0xa2, 0, |_| false).unwrap();
        allocate(&mut pool, 0xa3, 0, |_| false).unwrap();
        // 200 is the LRU but pressed: the next goes instead.
        let alloc = allocate(&mut pool, 0xb1, 1, |keycode| keycode == 200).unwrap();
        assert_eq!(alloc.keycode, 250);
        // Everything pressed: nothing to allocate, and the pool is unchanged.
        let before = pool.bindings().to_vec();
        assert_eq!(pool.candidate(|_| true), None);
        assert_eq!(pool.bindings(), before.as_slice());
    }

    #[test]
    fn reconcile_forgets_overwritten_bindings_and_rereads_spares() {
        let mut km = keymap();
        let mut pool = SpareKeycodes::new(&km);
        // Bindings are recorded under their row's first keysym.
        for row in [[0xa1, 0xa1], [0xe9, 0xc9]] {
            let alloc = allocate(&mut pool, row[0], 0, |_| false).unwrap();
            km.set(alloc.keycode, &row);
        }
        // Someone rebinds 250 and takes spare 100, xdotool's; the server reports ours on 200.
        km.set(250, &[0x41]);
        km.set(100, &[0x42]);
        km.set(200, &[0xe9, 0xc9]);
        pool.reconcile(&km);
        assert_eq!(
            pool.bindings()
                .iter()
                .map(|b| b.keycode)
                .collect::<Vec<_>>(),
            vec![200]
        );
        // The lowest spare now is 120, which is left to agents in turn.
        assert_eq!(pool.capacity(), 1);
        assert_eq!(
            allocate(&mut pool, 0xa3, 0, |_| false).unwrap().keycode,
            200
        );
    }

    #[test]
    fn reconcile_never_hands_out_a_held_keycode() {
        let mut km = keymap();
        let mut pool = SpareKeycodes::new(&km);
        let held = allocate(&mut pool, 0x0100_4f60, 0, |_| false)
            .unwrap()
            .keycode;
        km.set(held, &[0x0100_4f60, 0x0100_4f60]);
        // An agent's setxkbmap wipes the binding while the session still holds the key.
        km.set(held, &[N]);
        pool.reconcile(&km);
        assert!(pool.bindings().is_empty());
        let next = pool.candidate(|k| k == held).unwrap().keycode;
        assert_ne!(next, held);
        assert_eq!(next, 200);
    }
}
