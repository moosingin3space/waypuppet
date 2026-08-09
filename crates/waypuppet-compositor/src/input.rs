//! Turning names into keystrokes.
//!
//! The Wayland protocol carries *keycodes*, not keysyms. A caller writes
//! `ctrl+shift+t` or types `hello`, both of which are statements about keysyms,
//! so something has to search the keymap for a code that produces the wanted
//! symbol and work out which modifiers get it there. That search is this
//! module, done once at startup and answered from a table thereafter.
//!
//! # The layout is pinned
//!
//! [`LAYOUT`] is fixed rather than inherited from the environment. Chord parsing
//! resolves against the keymap, so a layout that varied with `XKB_DEFAULT_LAYOUT`
//! would make `waypuppet key at` mean different things on different machines —
//! precisely the nondeterminism the project exists to remove.
//!
//! # What cannot be typed is an error
//!
//! A character the layout cannot produce fails with
//! [`Failure::UnmappableChord`] rather than being dropped or approximated.
//! Silently typing something else would be a wrong answer that looks like a
//! right one, which is the worst thing this crate can do.

use std::collections::HashMap;

use smithay::input::keyboard::{Keycode, XkbConfig, xkb};
use waypuppet_proto::Failure;

/// The keyboard layout every session uses.
///
/// Stated rather than defaulted: see the module docs.
pub const LAYOUT: &str = "us";

/// The xkb configuration the seat is built with.
///
/// The seat and [`Keymap`] must agree, or the keycodes computed here would
/// resolve to different symbols on the client's copy of the keymap, so both
/// come from this one function.
#[must_use]
pub fn xkb_config() -> XkbConfig<'static> {
    XkbConfig {
        layout: LAYOUT,
        ..XkbConfig::default()
    }
}

/// Names a caller may reasonably use that xkb does not know.
///
/// xkb's own names win where they exist; this is only for the shorthands a
/// person actually types. Resolution is case-insensitive, so `Enter` and `enter`
/// both land here.
const ALIASES: &[(&str, &str)] = &[
    ("enter", "Return"),
    ("esc", "Escape"),
    ("del", "Delete"),
    ("ins", "Insert"),
    ("pgup", "Prior"),
    ("pagedown", "Next"),
    ("pgdn", "Next"),
    ("pageup", "Prior"),
    ("backspace", "BackSpace"),
    ("bs", "BackSpace"),
    ("caps", "Caps_Lock"),
];

/// A keystroke: one key, and the modifiers that must be held around it.
///
/// Modifiers are listed in press order; releasing walks the list backwards, so
/// a client sees the same nesting a human hand would produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stroke {
    /// Modifier keycodes to hold down, in press order.
    pub modifiers: Vec<Keycode>,
    /// The keycode to press and release.
    pub key: Keycode,
}

/// The modifier keys, by keycode, as far as the keymap has them.
#[derive(Debug, Clone, Copy, Default)]
struct ModifierKeys {
    ctrl: Option<Keycode>,
    alt: Option<Keycode>,
    shift: Option<Keycode>,
    logo: Option<Keycode>,
}

/// A modifier a chord can ask for.
///
/// The variants are declared in the order they are pressed, so sorting a
/// chord's modifiers by this enum gives the nesting a hand would produce, with
/// shift innermost — which also puts it next to the shift a shifted symbol
/// implies, so the two cannot end up on opposite sides of the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Modifier {
    Ctrl,
    Alt,
    Logo,
    Shift,
}

/// The reverse index: keysym to the keystroke that produces it.
///
/// Built once, from a keymap compiled with exactly the configuration the seat
/// uses. Lookups are then a hash probe rather than a search of the keymap.
#[derive(Debug)]
pub struct Keymap {
    /// Raw keysym to the keycode that produces it, and whether shift is needed.
    ///
    /// Populated unshifted-first, so a symbol reachable without shift is never
    /// recorded as needing it.
    strokes: HashMap<u32, (Keycode, bool)>,
    modifiers: ModifierKeys,
}

impl Keymap {
    /// Compile [`LAYOUT`] and index it.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Compositor`] if xkb cannot compile the layout,
    /// which means the xkb data files are missing rather than that anything
    /// waypuppet did was wrong.
    pub fn new() -> Result<Self, crate::Error> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let config = xkb_config();
        let keymap = xkb::Keymap::new_from_names(
            &context,
            config.rules,
            config.model,
            config.layout,
            config.variant,
            config.options.clone(),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| {
            crate::Error::Compositor(format!("xkb cannot compile the {LAYOUT:?} layout"))
        })?;

        let shift = 1_u32 << keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
        let mut state = xkb::State::new(&keymap);
        let mut strokes: HashMap<u32, (Keycode, bool)> = HashMap::new();

        // Unshifted first, so `or_insert` prefers the plainer way of reaching a
        // symbol. Asking the *state* what a keycode produces under a modifier
        // mask is the same computation the client will do with the same keymap,
        // which is why this is exact rather than a guess about level ordering.
        for shifted in [false, true] {
            let mask = if shifted { shift } else { 0 };
            state.update_mask(mask, 0, 0, 0, 0, 0);
            for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
                let keycode = Keycode::new(raw);
                for sym in state.key_get_syms(keycode) {
                    strokes.entry(sym.raw()).or_insert((keycode, shifted));
                }
            }
        }

        let lookup = |name: &str| {
            let sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
            strokes.get(&sym.raw()).map(|&(keycode, _)| keycode)
        };
        let modifiers = ModifierKeys {
            ctrl: lookup("Control_L"),
            alt: lookup("Alt_L"),
            shift: lookup("Shift_L"),
            logo: lookup("Super_L"),
        };

        Ok(Self { strokes, modifiers })
    }

    /// Resolve a chord such as `ctrl+shift+t`, `Return` or `F5`.
    ///
    /// # Errors
    ///
    /// Returns [`Failure::UnmappableChord`] if a modifier name is not
    /// recognised, the key name resolves to no keysym, or the keymap has no
    /// keycode that produces it.
    pub fn chord(&self, chord: &str) -> Result<Stroke, Failure> {
        let unmappable = || Failure::UnmappableChord {
            chord: chord.to_owned(),
        };

        let (wanted, name) = split(chord).ok_or_else(unmappable)?;
        let keysym = resolve_name(name).ok_or_else(unmappable)?;
        self.stroke(keysym, &wanted).ok_or_else(unmappable)
    }

    /// Resolve a single character, as `type` needs one per character.
    ///
    /// # Errors
    ///
    /// Returns [`Failure::UnmappableChord`] if [`LAYOUT`] cannot produce the
    /// character. Erroring is the point: typing something else instead would be
    /// a wrong answer wearing a right answer's clothes.
    pub fn character(&self, ch: char) -> Result<Stroke, Failure> {
        // A newline in a string means "press Return", not "type U+000A" — no
        // layout has a key for the control character itself.
        let keysym = match ch {
            '\n' | '\r' => xkb::keysym_from_name("Return", xkb::KEYSYM_NO_FLAGS),
            '\t' => xkb::keysym_from_name("Tab", xkb::KEYSYM_NO_FLAGS),
            other => xkb::utf32_to_keysym(other as u32),
        };
        self.stroke(keysym, &[])
            .ok_or_else(|| Failure::UnmappableChord {
                chord: ch.to_string(),
            })
    }

    /// Build the keystroke for a keysym, folding in explicitly asked-for
    /// modifiers as well as the shift the symbol itself needs.
    fn stroke(&self, keysym: xkb::Keysym, wanted: &[Modifier]) -> Option<Stroke> {
        let &(key, needs_shift) = self.strokes.get(&keysym.raw())?;

        let mut held: Vec<Modifier> = wanted.to_vec();
        // `A` is shift-`a`, so the symbol brings its own shift. Asking for
        // `shift+A` as well must not press shift twice.
        if needs_shift && !held.contains(&Modifier::Shift) {
            held.push(Modifier::Shift);
        }
        held.sort_unstable();
        held.dedup();

        // A modifier named in a chord must exist in the keymap. Refusing here
        // rather than dropping it is the same rule as an unproducible
        // character: `ctrl+c` that quietly sends a bare `c` is a wrong answer.
        let modifiers = held
            .into_iter()
            .map(|modifier| match modifier {
                Modifier::Ctrl => self.modifiers.ctrl,
                Modifier::Alt => self.modifiers.alt,
                Modifier::Logo => self.modifiers.logo,
                Modifier::Shift => self.modifiers.shift,
            })
            .collect::<Option<Vec<_>>>()?;

        Some(Stroke { modifiers, key })
    }
}

/// Split a chord into its modifiers and its key name.
///
/// `+` is both the separator and a key name, so a chord that ends in one — the
/// literal plus key, as in `ctrl++` — is read as naming it.
fn split(chord: &str) -> Option<(Vec<Modifier>, &str)> {
    let chord = chord.trim();
    if chord.is_empty() {
        return None;
    }

    let mut parts: Vec<&str> = chord.split('+').collect();
    // A trailing empty part means the chord ended with the separator, so the
    // key is the plus itself.
    let key = match parts.pop()? {
        "" if !parts.is_empty() => {
            parts.pop()?;
            "plus"
        }
        name => name,
    };
    if key.is_empty() {
        return None;
    }

    let wanted = parts
        .into_iter()
        .map(|part| match part.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Some(Modifier::Ctrl),
            "alt" | "mod1" | "meta" => Some(Modifier::Alt),
            "shift" => Some(Modifier::Shift),
            "super" | "logo" | "win" | "cmd" => Some(Modifier::Logo),
            // An unknown modifier is a typo, and sending the rest of the chord
            // without it would be a wrong answer.
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some((wanted, key))
}

/// Resolve a key name to a keysym, trying the friendly spellings too.
fn resolve_name(name: &str) -> Option<xkb::Keysym> {
    let exact = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
    if exact.raw() != 0 {
        return Some(exact);
    }

    let lowered = name.to_ascii_lowercase();
    if let Some((_, canonical)) = ALIASES.iter().find(|(alias, _)| *alias == lowered) {
        let aliased = xkb::keysym_from_name(canonical, xkb::KEYSYM_NO_FLAGS);
        if aliased.raw() != 0 {
            return Some(aliased);
        }
    }

    // Last, because it is the loosest: `escape` and `f5` should work, but only
    // once the exact spellings have had their chance.
    let insensitive = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
    (insensitive.raw() != 0).then_some(insensitive)
}

#[cfg(test)]
mod tests {
    use super::{Keymap, Modifier, split};

    /// Every assertion here is about the pinned `us` layout, so building the
    /// keymap once per test is the whole setup.
    fn keymap() -> Keymap {
        Keymap::new().expect("the us layout compiles")
    }

    #[test]
    fn a_plain_letter_needs_no_modifiers() {
        let stroke = keymap().chord("a").expect("`a` is on every keyboard");
        assert!(
            stroke.modifiers.is_empty(),
            "lowercase `a` should not hold shift: {stroke:?}",
        );
    }

    #[test]
    fn a_capital_letter_brings_its_own_shift() {
        let keymap = keymap();
        let lower = keymap.chord("a").expect("a");
        let upper = keymap.chord("A").expect("A");

        assert_eq!(
            lower.key, upper.key,
            "the same physical key produces both cases",
        );
        assert_eq!(
            upper.modifiers.len(),
            1,
            "`A` is reached by shifting `a`: {upper:?}",
        );
    }

    #[test]
    fn a_named_key_resolves_however_it_is_spelled() {
        let keymap = keymap();
        let canonical = keymap.chord("Return").expect("Return");
        for spelling in ["return", "enter", "Enter"] {
            assert_eq!(
                keymap.chord(spelling).expect(spelling).key,
                canonical.key,
                "{spelling} should be the same key as Return",
            );
        }
    }

    #[test]
    fn modifiers_are_held_in_press_order() {
        let stroke = keymap().chord("ctrl+shift+t").expect("ctrl+shift+t");
        assert_eq!(
            stroke.modifiers.len(),
            2,
            "both named modifiers are held: {stroke:?}",
        );
    }

    #[test]
    fn an_unknown_modifier_is_refused_rather_than_ignored() {
        assert!(
            keymap().chord("hyper+t").is_err(),
            "an unrecognised modifier must not be silently dropped",
        );
    }

    #[test]
    fn the_plus_key_survives_being_the_separator() {
        let (wanted, key) = split("ctrl++").expect("ctrl++ parses");
        assert_eq!(wanted, vec![Modifier::Ctrl], "the modifier is still read");
        assert_eq!(key, "plus", "the trailing separator names the plus key");

        assert!(
            keymap().chord("ctrl++").is_ok(),
            "ctrl++ should resolve to a real keystroke",
        );
    }

    #[test]
    fn a_shifted_character_types_with_shift() {
        let stroke = keymap().character('!').expect("! is on the us layout");
        assert_eq!(
            stroke.modifiers.len(),
            1,
            "`!` is shift-1 on the us layout: {stroke:?}",
        );
    }

    #[test]
    fn a_newline_types_return() {
        let keymap = keymap();
        assert_eq!(
            keymap.character('\n').expect("newline").key,
            keymap.chord("Return").expect("Return").key,
            "a newline in a typed string means the Return key",
        );
    }

    #[test]
    fn a_character_the_layout_cannot_produce_is_an_error() {
        // Nothing on `us` types a CJK ideograph, and pretending otherwise would
        // put the wrong text in the application under test.
        assert!(
            keymap().character('漢').is_err(),
            "an unproducible character must fail rather than type something else",
        );
    }

    #[test]
    fn a_bare_plus_is_the_plus_key() {
        let (wanted, key) = split("+").expect("+ parses");
        assert_eq!(
            (wanted, key),
            (Vec::new(), "plus"),
            "`+` on its own names a key rather than separating nothing",
        );
    }

    #[test]
    fn a_symbol_that_already_shifts_does_not_shift_twice() {
        let stroke = keymap().chord("shift+A").expect("shift+A");
        assert_eq!(
            stroke.modifiers.len(),
            1,
            "`A` brings its own shift; asking again must not press two: {stroke:?}",
        );
    }

    #[test]
    fn an_empty_chord_is_not_a_chord() {
        assert_eq!(split(""), None, "there is no key called nothing");
        assert!(keymap().chord("").is_err());
    }
}
