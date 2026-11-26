//! Idempotent updates of the annotation line in activity descriptions.

use regex::{escape, Regex};

use crate::config::Settings;

fn line_pattern(settings: &Settings) -> Regex {
    let emoji = r"[\u{2600}-\u{27bf}\u{1f000}-\u{1faff}][\u{2600}-\u{27bf}\u{1f000}-\u{1faff}\u{fe0f}\u{200d}]*";
    let marker = if settings.description_emoji.is_empty() {
        emoji.to_owned()
    } else {
        format!("(?:{}|{emoji})", escape(&settings.description_emoji))
    };
    Regex::new(&format!(
        r"(?m)^[ \t]*(?:{marker}[ \t]+)?{}:[ \t].*$",
        escape(&settings.description_prefix)
    ))
    .expect("escaped description settings produce a valid regex")
}

// Python splitlines recognizes additional Unicode and ASCII line boundaries,
// treats CRLF as one boundary, and omits the empty field after a final boundary.
fn split_lines(mut input: &str) -> impl Iterator<Item = &str> {
    std::iter::from_fn(move || {
        if input.is_empty() {
            return None;
        }
        let Some((index, separator)) = input.char_indices().find(|(_, ch)| {
            matches!(
                ch,
                '\n' | '\r' | '\x0b' | '\x0c' | '\x1c'
                    ..='\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}'
            )
        }) else {
            return Some(std::mem::take(&mut input));
        };
        let line = &input[..index];
        input = &input[index + separator.len_utf8()..];
        if separator == '\r' {
            input = input.strip_prefix('\n').unwrap_or(input);
        }
        Some(line)
    })
}

pub struct DescriptionMatcher {
    pattern: Regex,
}

impl DescriptionMatcher {
    pub fn new(settings: &Settings) -> Self {
        Self {
            pattern: line_pattern(settings),
        }
    }
    pub fn update(&self, description: Option<&str>, new_line: &str) -> String {
        let description = description.unwrap_or_default();
        if description.is_empty() {
            return new_line.to_owned();
        }

        let pattern = &self.pattern;
        if !pattern.is_match(description) {
            let separator = if description.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            return format!("{description}{separator}{new_line}");
        }

        let mut replacement = Some(new_line);
        split_lines(description)
            .filter_map(|line| {
                if pattern.find(line).is_some_and(|found| found.start() == 0) {
                    replacement.take()
                } else {
                    Some(line)
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn remove(&self, description: Option<&str>) -> String {
        self.pattern
            .replace_all(description.unwrap_or_default(), "")
            .trim_end_matches('\n')
            .to_owned()
    }

    pub fn has(&self, description: Option<&str>) -> bool {
        description.is_some_and(|description| self.pattern.is_match(description))
    }
}

pub fn update_description_line(
    settings: &Settings,
    description: Option<&str>,
    new_line: &str,
) -> String {
    DescriptionMatcher::new(settings).update(description, new_line)
}

pub fn remove_description_line(settings: &Settings, description: Option<&str>) -> String {
    DescriptionMatcher::new(settings).remove(description)
}

pub fn has_description_line(settings: &Settings, description: Option<&str>) -> bool {
    DescriptionMatcher::new(settings).has(description)
}

#[cfg(test)]
mod tests {

    #[test]
    fn matcher_reuses_pattern_across_descriptions() {
        let settings = Settings::default();
        let matcher = DescriptionMatcher::new(&settings);
        let line = format!(
            "{} {}: new",
            settings.description_emoji, settings.description_prefix
        );
        for text in ["first ride", "second ride"] {
            let updated = matcher.update(Some(text), &line);
            assert!(matcher.has(Some(&updated)));
            assert_eq!(matcher.update(Some(&updated), &line), updated);
            assert_eq!(matcher.remove(Some(&updated)), text);
        }
    }

    use super::*;

    #[test]
    fn replace_first_marker_and_collapse_duplicates() {
        let settings = Settings {
            description_prefix: "Tile[Harvester]".into(),
            description_emoji: "(map)".into(),
            ..Settings::default()
        };
        let description = "Prose\r\n \t🚴 Tile[Harvester]: old\r\nMiddle\u{2028}Next\r\n(map) Tile[Harvester]:\tduplicate\r\nTile[Harvester]: plain\n";
        let new = "(map) Tile[Harvester]: new";
        assert!(has_description_line(&settings, Some(description)));
        assert_eq!(
            update_description_line(&settings, Some(description), new),
            "Prose\n(map) Tile[Harvester]: new\nMiddle\nNext"
        );
    }

    #[test]
    fn preserve_punctuation_and_prose() {
        let settings = Settings::default();
        let new = "🗺️ TileHarvester: new";
        for description in [
            "— TileHarvester: prose",
            "- TileHarvester: prose",
            "* TileHarvester: prose",
            "... TileHarvester: prose",
            "> TileHarvester: prose",
            "I tried TileHarvester: it helped me",
            "TileHarvester:no space",
        ] {
            assert!(!has_description_line(&settings, Some(description)));
            assert_eq!(
                remove_description_line(&settings, Some(description)),
                description
            );
            assert_eq!(
                update_description_line(&settings, Some(description), new),
                format!("{description}\n\n{new}")
            );
        }
    }

    #[test]
    fn match_python_newline_rules() {
        let settings = Settings::default();
        let new = "TileHarvester: new";
        for empty in [None, Some("")] {
            assert_eq!(update_description_line(&settings, empty, new), new);
            assert_eq!(remove_description_line(&settings, empty), "");
            assert!(!has_description_line(&settings, empty));
        }
        assert_eq!(
            update_description_line(&settings, Some("Ride\n"), new),
            "Ride\n\nTileHarvester: new"
        );
        assert_eq!(
            remove_description_line(&settings, Some("Ride\r\n🗺️ TileHarvester: old\r\n\n")),
            "Ride\r"
        );
        assert_eq!(
            remove_description_line(&settings, Some("Before\nTileHarvester: old\nAfter\n")),
            "Before\n\nAfter"
        );
    }
}
