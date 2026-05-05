//! Name generators for vault goals and intents.
//!
//! Goals get Docker-style haikunator names (adjective-noun-4hex).
//! Intent IDs use JIRA-style prefixes (PREFIX-N).

use rand::Rng;

// Word lists for goal names (adjective-noun pattern)

const ADJECTIVES: &[&str] = &[
    "aged",
    "ancient",
    "autumn",
    "bold",
    "brave",
    "bright",
    "broad",
    "calm",
    "cold",
    "cool",
    "crimson",
    "curly",
    "damp",
    "dark",
    "dawn",
    "deep",
    "delicate",
    "divine",
    "dry",
    "eager",
    "early",
    "empty",
    "falling",
    "feral",
    "fierce",
    "flat",
    "floral",
    "fragrant",
    "free",
    "fresh",
    "frosty",
    "gentle",
    "golden",
    "green",
    "grim",
    "hidden",
    "hollow",
    "holy",
    "hushed",
    "icy",
    "jolly",
    "keen",
    "kind",
    "late",
    "light",
    "lingering",
    "little",
    "lively",
    "long",
    "lucky",
    "misty",
    "morning",
    "muddy",
    "mute",
    "nameless",
    "noble",
    "odd",
    "old",
    "orange",
    "patient",
    "plain",
    "polished",
    "proud",
    "purple",
    "quiet",
    "rapid",
    "raspy",
    "red",
    "restless",
    "rich",
    "rough",
    "round",
    "royal",
    "rustic",
    "sandy",
    "shy",
    "silent",
    "small",
    "snowy",
    "soft",
    "solitary",
    "sparkling",
    "spring",
    "steep",
    "still",
    "summer",
    "swift",
    "tall",
    "throbbing",
    "tight",
    "tiny",
    "twilight",
    "wandering",
    "weathered",
    "white",
    "wild",
    "winter",
    "wispy",
    "withered",
    "young",
];

const NOUNS: &[&str] = &[
    "bird",
    "bloom",
    "boulder",
    "breeze",
    "brook",
    "bush",
    "butterfly",
    "canyon",
    "cave",
    "cherry",
    "cliff",
    "cloud",
    "creek",
    "dawn",
    "dew",
    "dream",
    "dust",
    "feather",
    "field",
    "fire",
    "firefly",
    "flame",
    "flower",
    "fog",
    "forest",
    "frog",
    "frost",
    "garden",
    "glade",
    "grass",
    "grove",
    "haze",
    "hill",
    "lake",
    "leaf",
    "log",
    "marsh",
    "meadow",
    "moon",
    "morning",
    "moss",
    "mountain",
    "needle",
    "night",
    "oak",
    "ocean",
    "paper",
    "pebble",
    "pine",
    "plain",
    "pond",
    "rain",
    "resonance",
    "ridge",
    "river",
    "rock",
    "rose",
    "sand",
    "sea",
    "shadow",
    "shape",
    "silence",
    "sky",
    "smoke",
    "snow",
    "snowflake",
    "sound",
    "spring",
    "star",
    "stone",
    "storm",
    "stream",
    "sun",
    "sunset",
    "surf",
    "thunder",
    "tree",
    "truth",
    "violet",
    "voice",
    "water",
    "waterfall",
    "wave",
    "wave",
    "wildflower",
    "wind",
    "wood",
];

/// Generate a Docker-style goal name: adjective-noun-4hex.
///
/// Examples: "swift-meadow-a3f2", "calm-river-b7e1", "brave-sunset-04dc"
///
/// The 4 hex suffix ensures uniqueness even if the same adjective-noun
/// pair is drawn twice (~65K combinations per pair).
pub fn generate_goal_name() -> String {
    let mut rng = rand::thread_rng();

    let adj = ADJECTIVES[rng.gen_range(0..ADJECTIVES.len())];
    let noun = NOUNS[rng.gen_range(0..NOUNS.len())];
    let token: u16 = rng.gen();

    format!("{}-{}-{:04x}", adj, noun, token)
}

/// Derive a JIRA-style intent prefix from a project directory name.
///
/// Takes the first 4 alphanumeric characters and uppercases them.
/// Examples: "pi-mono-rs" → "PIMO", "atomic" → "ATOM"
pub fn derive_intent_prefix(project_name: &str) -> String {
    project_name
        .chars()
        .filter(|c| c.is_alphanumeric())
        .take(4)
        .collect::<String>()
        .to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_name_format() {
        let name = generate_goal_name();
        let parts: Vec<&str> = name.split('-').collect();
        // Should be adjective-noun-hex (3 parts)
        assert_eq!(parts.len(), 3, "name '{}' should have 3 parts", name);
        // Last part should be 4 hex chars
        assert_eq!(parts[2].len(), 4);
        assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn goal_names_are_unique() {
        // Generate 100 names and verify no duplicates
        let names: Vec<String> = (0..100).map(|_| generate_goal_name()).collect();
        let unique: std::collections::HashSet<&String> = names.iter().collect();
        // With ~8500 combinations (adjectives × nouns) × 65536 tokens,
        // collisions in 100 samples are astronomically unlikely
        assert_eq!(names.len(), unique.len());
    }

    #[test]
    fn intent_prefix_derivation() {
        assert_eq!(derive_intent_prefix("pi-mono-rs"), "PIMO");
        assert_eq!(derive_intent_prefix("atomic"), "ATOM");
        assert_eq!(derive_intent_prefix("my-app"), "MYAP");
        assert_eq!(derive_intent_prefix("a"), "A");
        assert_eq!(derive_intent_prefix(""), "");
        assert_eq!(derive_intent_prefix("hello_world_project"), "HELL");
        assert_eq!(derive_intent_prefix("123-numbers"), "123N");
    }
}
