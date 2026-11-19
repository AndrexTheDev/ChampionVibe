//! Camelot-wheel mapping shared by the importer, the mix engine and the grid editor.
//!
//! The harmonic-mixing wheel numbers every key 1..=12 twice: `A` for minor, `B` for major. The
//! mapping is pure data (no allocation, no I/O), so both the analysis pipeline and a manual
//! correction can be normalised to the same spelling before it is stored.

/// Minor keys, wheel positions 1A..=12A.
const MINOR_WHEEL: [(&str, &str); 17] = [
    ("G#", "1A"),
    ("Ab", "1A"),
    ("D#", "2A"),
    ("Eb", "2A"),
    ("A#", "3A"),
    ("Bb", "3A"),
    ("F", "4A"),
    ("C", "5A"),
    ("G", "6A"),
    ("D", "7A"),
    ("A", "8A"),
    ("E", "9A"),
    ("B", "10A"),
    ("F#", "11A"),
    ("Gb", "11A"),
    ("C#", "12A"),
    ("Db", "12A"),
];

/// Major keys, wheel positions 1B..=12B.
const MAJOR_WHEEL: [(&str, &str); 18] = [
    ("B", "1B"),
    ("Cb", "1B"),
    ("F#", "2B"),
    ("Gb", "2B"),
    ("C#", "3B"),
    ("Db", "3B"),
    ("G#", "4B"),
    ("Ab", "4B"),
    ("D#", "5B"),
    ("Eb", "5B"),
    ("A#", "6B"),
    ("Bb", "6B"),
    ("F", "7B"),
    ("C", "8B"),
    ("G", "9B"),
    ("D", "10B"),
    ("A", "11B"),
    ("E", "12B"),
];

/// Parses a ChampionVibe key spelling such as `A minor`, `Bb major` or `F# minor` into its tonic
/// (pitch-class spelling) and mode. Flats are normalised to the sharp spelling used by the
/// analyser, so `Bb minor` and `A# minor` resolve to the same wheel position.
pub fn parse_musical_key(value: &str) -> Option<(String, bool)> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let tonic = parts.next()?;
    let mode = parts.next().unwrap_or("major").to_ascii_lowercase();
    if parts.next().is_some() {
        return None;
    }
    let tonic = normalise_tonic(tonic)?;
    match mode.as_str() {
        "major" | "maj" | "ionian" => Some((tonic, true)),
        "minor" | "min" | "aeolian" => Some((tonic, false)),
        _ => None,
    }
}

/// Normalises `b`/`#` accidentals onto the spelling the analyser produces (`C`, `C#`, `D`, …).
pub fn normalise_tonic(tonic: &str) -> Option<String> {
    let mut characters = tonic.chars();
    let letter = characters.next()?.to_ascii_uppercase();
    if !('A'..='G').contains(&letter) {
        return None;
    }
    let mut accidental = String::new();
    for character in characters {
        match character {
            '#' => accidental.push('#'),
            'b' | 'B' => accidental.push('b'),
            _ => return None,
        }
    }
    Some(format!("{letter}{accidental}"))
}

/// Camelot position for a spelled tonic and mode, if the tonic belongs to the 12-tone set.
fn wheel_position(tonic: &str, major: bool) -> Option<&'static str> {
    let sharp = to_sharp_tonic(tonic)?;
    let table: &[(&str, &str)] = if major { &MAJOR_WHEEL } else { &MINOR_WHEEL };
    table
        .iter()
        .find(|(name, _)| *name == sharp)
        .map(|(_, camelot)| *camelot)
}

/// Converts a flat spelling into its sharp equivalent so one table entry per pitch class suffices.
fn to_sharp_tonic(tonic: &str) -> Option<&'static str> {
    const SHARP_EQUIVALENTS: [(&str, &str); 8] = [
        ("Db", "C#"),
        ("Eb", "D#"),
        ("Gb", "F#"),
        ("Ab", "G#"),
        ("Bb", "A#"),
        ("Cb", "B"),
        ("Fb", "E"),
        ("E#", "F"),
    ];
    const NATURALS: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    if let Some((_, sharp)) = SHARP_EQUIVALENTS.iter().find(|(flat, _)| *flat == tonic) {
        return Some(sharp);
    }
    NATURALS.iter().find(|name| **name == tonic).copied()
}

/// Camelot code for a musical-key spelling, e.g. `A minor` → `8A`, `C major` → `8B`.
pub fn camelot_for_musical_key(value: &str) -> Option<&'static str> {
    let (tonic, major) = parse_musical_key(value)?;
    wheel_position(&tonic, major)
}

/// Musical key spelling for a Camelot code, e.g. `8A` → `A minor`, `8B` → `C major`.
pub fn musical_key_for_camelot(value: &str) -> Option<String> {
    let code = value.trim().to_ascii_uppercase();
    let (digits, letter) = code.split_at(code.len().checked_sub(1)?);
    let position: u8 = digits.parse().ok()?;
    if !(1..=12).contains(&position) {
        return None;
    }
    let high_energy = matches!(letter, "A" | "B");
    if !high_energy {
        return None;
    }
    Some(match (position, letter) {
        (1, "A") => "G# minor".to_owned(),
        (2, "A") => "D# minor".to_owned(),
        (3, "A") => "A# minor".to_owned(),
        (4, "A") => "F minor".to_owned(),
        (5, "A") => "C minor".to_owned(),
        (6, "A") => "G minor".to_owned(),
        (7, "A") => "D minor".to_owned(),
        (8, "A") => "A minor".to_owned(),
        (9, "A") => "E minor".to_owned(),
        (10, "A") => "B minor".to_owned(),
        (11, "A") => "F# minor".to_owned(),
        (12, "A") => "C# minor".to_owned(),
        (1, _) => "B major".to_owned(),
        (2, _) => "F# major".to_owned(),
        (3, _) => "C# major".to_owned(),
        (4, _) => "G# major".to_owned(),
        (5, _) => "D# major".to_owned(),
        (6, _) => "A# major".to_owned(),
        (7, _) => "F major".to_owned(),
        (8, _) => "C major".to_owned(),
        (9, _) => "G major".to_owned(),
        (10, _) => "D major".to_owned(),
        (11, _) => "A major".to_owned(),
        (12, _) => "E major".to_owned(),
        // The range check above makes every other pair unreachable; the arm keeps the match total.
        _ => return None,
    })
}

/// The neighbouring wheel positions that mix harmonically: the same number with the other mode,
/// one step up/down, and the ±7 semitone relative (which is the same wheel number).
pub fn compatible_camelot(value: &str) -> Vec<String> {
    let code = value.trim().to_ascii_uppercase();
    if code.len() < 2 {
        return Vec::new();
    }
    let (digits, letter) = code.split_at(code.len() - 1);
    let Ok(position) = digits.parse::<i32>() else {
        return Vec::new();
    };
    if !(1..=12).contains(&position) || !matches!(letter, "A" | "B") {
        return Vec::new();
    }
    let other = if letter == "A" { "B" } else { "A" };
    let step = |offset: i32| ((position - 1 + offset).rem_euclid(12)) + 1;
    vec![
        format!("{position}{other}"),
        format!("{}{letter}", step(1)),
        format!("{}{letter}", step(-1)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_analyser_spellings_onto_the_wheel() {
        assert_eq!(camelot_for_musical_key("A minor"), Some("8A"));
        assert_eq!(camelot_for_musical_key("C major"), Some("8B"));
        assert_eq!(camelot_for_musical_key("C# minor"), Some("12A"));
        assert_eq!(camelot_for_musical_key("Bb major"), Some("6B"));
        assert_eq!(camelot_for_musical_key("F# major"), Some("2B"));
        assert_eq!(camelot_for_musical_key("H minor"), None);
        assert_eq!(camelot_for_musical_key(""), None);
    }

    #[test]
    fn flats_and_sharps_share_one_wheel_position() {
        assert_eq!(
            camelot_for_musical_key("Bb minor"),
            camelot_for_musical_key("A# minor")
        );
        assert_eq!(
            camelot_for_musical_key("Gb major"),
            camelot_for_musical_key("F# major")
        );
    }

    #[test]
    fn camelot_codes_round_trip_through_the_wheel() {
        for position in 1..=12 {
            for letter in ["A", "B"] {
                let code = format!("{position}{letter}");
                let key = musical_key_for_camelot(&code).expect("code maps to a key");
                assert_eq!(
                    camelot_for_musical_key(&key).as_deref(),
                    Some(code.as_str()),
                    "round trip for {code} via {key}"
                );
            }
        }
        assert_eq!(musical_key_for_camelot("13A"), None);
        assert_eq!(musical_key_for_camelot("8C"), None);
        assert_eq!(musical_key_for_camelot(""), None);
    }

    #[test]
    fn compatible_positions_stay_on_the_wheel() {
        assert_eq!(compatible_camelot("8A"), vec!["8B", "9A", "7A"]);
        assert_eq!(compatible_camelot("12B"), vec!["12A", "1B", "11B"]);
        assert!(compatible_camelot("8X").is_empty());
    }
}
