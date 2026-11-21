//! `block.properties` (spec 4.1): `block.<id>=<block> <block> ...`, each block
//! `[namespace:]name` with optional state filters `:<property>=<v1>,<v2>`. Values in one filter
//! are alternatives; separate filters must all match. Names the game does not know are simply
//! never asked for. Block tags (`%tag`, tier E) resolve against the current world's tags.

use crate::Properties;

#[derive(Clone, Debug, PartialEq)]
struct Rule {
    id: i32,
    /// `minecraft:stone`.
    name: String,
    filters: Vec<(String, Vec<String>)>,
    from_tag: bool,
}

/// The ids `block.properties` gives block states.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlockIds {
    rules: Vec<Rule>,
    /// Unexpanded declarations, retained so a reload can replace all tag members.
    tags: Vec<Rule>,
    /// Lines skipped and why.
    pub notes: Vec<String>,
}

impl BlockIds {
    pub fn parse(properties: &Properties) -> Self {
        Self::parse_kind(properties, "block")
    }

    /// The same for `<kind>.<id>=` lines: `entity` in `entity.properties`, `item` in
    /// `item.properties` (4.1); names there carry no state filters.
    pub fn parse_kind(properties: &Properties, kind: &str) -> Self {
        let mut out = Self::default();
        let prefix = format!("{kind}.");
        for e in &properties.entries {
            let Some(id) = e.key.strip_prefix(prefix.as_str()) else {
                continue;
            };
            let Ok(id) = id.parse::<i32>() else {
                // `layer.*` (4.2) and other keys are not ids.
                continue;
            };
            for word in e.value.split_whitespace() {
                let (word, from_tag) = match word.strip_prefix('%') {
                    Some(tag) if kind == "block" => (tag, true),
                    Some(_) => {
                        out.notes.push(format!(
                            "line {}: tags are only supported for blocks: {word}",
                            e.line
                        ));
                        continue;
                    }
                    None => (word, false),
                };
                let (names, filters): (Vec<&str>, Vec<&str>) =
                    word.split(':').partition(|p| !p.contains('='));
                let name = match names[..] {
                    [name] => format!("minecraft:{name}"),
                    [namespace, name] => format!("{namespace}:{name}"),
                    _ => {
                        out.notes
                            .push(format!("line {}: not a block name: {word}", e.line));
                        continue;
                    }
                };
                let filters = filters
                    .iter()
                    .filter_map(|f| f.split_once('='))
                    .map(|(k, v)| (k.to_owned(), v.split(',').map(str::to_owned).collect()))
                    .collect();
                let rule = Rule {
                    id,
                    name,
                    filters,
                    from_tag,
                };
                if from_tag {
                    out.tags.push(rule);
                } else {
                    out.rules.push(rule);
                }
            }
        }
        out
    }

    /// `block.properties` after Iris's fix-ups: a `\` continuation across a blank line joins
    /// the lines, and a `block.<id>=` key that follows another entry on the same line starts
    /// a line of its own. Run it on the file's text before [`Properties::parse`].
    pub fn normalize(text: &str) -> String {
        let text = text.replace("\r\n", "\n");
        // `\` + newline + blank lines: the value goes on with the next non-blank line.
        let mut joined = String::with_capacity(text.len());
        let mut rest = text.as_str();
        while let Some(at) = rest.find("\\\n") {
            joined.push_str(&rest[..at]);
            let after = &rest[at + 2..];
            // Whitespace-only lines right after the backslash.
            let mut skipped = 0;
            let mut blank = false;
            for line in after.split_inclusive('\n') {
                if line.trim().is_empty() && line.ends_with('\n') {
                    skipped += line.len();
                    blank = true;
                } else {
                    break;
                }
            }
            if blank {
                joined.push(' ');
                rest = &after[skipped..];
            } else {
                joined.push_str("\\\n");
                rest = after;
            }
        }
        joined.push_str(rest);
        // `... block.12=...` on one line: break before the second key.
        let mut out = String::with_capacity(joined.len());
        for line in joined.split_inclusive('\n') {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') || !trimmed.contains("block.") {
                out.push_str(line);
                continue;
            }
            let bytes = line.as_bytes();
            let mut last = 0;
            let mut from = 0;
            while let Some(i) = line[from..].find("block.").map(|i| i + from) {
                from = i + 6;
                let digits = line[from..]
                    .trim_start_matches('-')
                    .bytes()
                    .take_while(u8::is_ascii_digit)
                    .count();
                let key_end = from + (line[from..].len() - line[from..].trim_start_matches('-').len()) + digits;
                let is_key = digits > 0 && line[key_end..].trim_start().starts_with('=');
                // Only after another word, and not as the tail of a longer name.
                let before = line[..i].trim_end_matches([' ', '\t']);
                let spaced = i == 0 || bytes[i - 1] == b' ' || bytes[i - 1] == b'\t';
                if is_key && spaced && !before.is_empty() && !before.ends_with('\\') && !before.ends_with('=') {
                    out.push_str(&line[last..before.len()]);
                    out.push('\n');
                    last = i;
                }
            }
            out.push_str(&line[last..]);
        }
        out
    }

    /// Iris's `LegacyIdMap`: the old numeric ids a pack without `block.properties` reads.
    pub fn legacy() -> Self {
        const COLORS: [&str; 16] = [
            "white", "orange", "magenta", "light_blue", "yellow", "lime", "pink", "gray",
            "light_gray", "cyan", "purple", "blue", "brown", "green", "red", "black",
        ];
        const WOODS: [&str; 6] = ["oak", "birch", "jungle", "spruce", "acacia", "dark_oak"];
        let colored = |suffix: &str| {
            COLORS
                .iter()
                .map(|c| format!("{c}_{suffix}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let leaves = WOODS
            .iter()
            .map(|w| format!("{w}_leaves"))
            .collect::<Vec<_>>()
            .join(" ");
        let text = format!(
            "block.1=stone granite diorite andesite\nblock.2=grass_block\nblock.4=cobblestone\n\
             block.50=torch\nblock.89=glowstone\nblock.124=redstone_lamp\nblock.12=sand\n\
             block.24=sandstone\nblock.41=gold_block\nblock.42=iron_block\nblock.57=diamond_block\n\
             block.-123=emerald_block\nblock.35={}\nblock.9=water\nblock.11=lava\nblock.79=ice\n\
             block.18={leaves}\nblock.95={}\nblock.160={}\n\
             block.31=grass seagrass sweet_berry_bush\nblock.59=wheat carrots potatoes\n\
             block.37=dandelion poppy blue_orchid allium azure_bluet red_tulip pink_tulip white_tulip \
             orange_tulip oxeye_daisy cornflower lily_of_the_valley wither_rose\n\
             block.175=sunflower lilac tall_grass large_fern rose_bush peony tall_seagrass\n\
             block.51=fire\nblock.111=lily_pad\n",
            colored("wool"),
            colored("stained_glass"),
            colored("stained_glass_pane"),
        );
        Self::parse(&Properties::parse(&text, &crate::Macros::standard()))
    }

    /// The ids a pack's `block.properties` text gives (after [`Self::normalize`]); a pack
    /// without the file (`None`) reads Iris's legacy numeric ids ([`Self::legacy`]).
    pub fn from_pack_text(text: Option<&str>, macros: &crate::Macros) -> Self {
        match text {
            Some(text) => Self::parse(&Properties::parse(&Self::normalize(text), macros)),
            None => Self::legacy(),
        }
    }

    /// Resolve `%tag` declarations against a world's current block tags. Re-resolving
    /// replaces earlier members; missing tags contribute no assignments.
    pub fn with_block_tags(
        &self,
        tags: &std::collections::HashMap<String, std::collections::HashSet<String>>,
    ) -> Self {
        let mut out = self.clone();
        out.rules.retain(|r| !r.from_tag);
        for rule in &self.tags {
            for member in tags.get(&rule.name).into_iter().flatten() {
                let mut expanded = rule.clone();
                expanded.name.clone_from(member);
                out.rules.push(expanded);
            }
        }
        out
    }

    pub fn has_block_tags(&self) -> bool {
        !self.tags.is_empty()
    }

    /// The id of block `name` (`minecraft:stone`) in the state with `properties`. The rule with
    /// the most filters that all match wins, then the first in the file. Matching individual
    /// assignments take priority over tag assignments, regardless of filter count.
    pub fn id<'a>(&self, name: &str, properties: impl Fn(&str) -> Option<&'a str>) -> Option<i32> {
        self.rules
            .iter()
            .filter(|r| r.name == name)
            .filter(|r| {
                r.filters
                    .iter()
                    .all(|(k, vs)| properties(k).is_some_and(|v| vs.iter().any(|x| x == v)))
            })
            .min_by_key(|r| (r.from_tag, std::cmp::Reverse(r.filters.len())))
            .map(|r| r.id)
    }

    /// Every name without state filters and its id (the first line naming it wins): what
    /// `entity.properties` and `item.properties` hold.
    pub fn plain(&self) -> std::collections::HashMap<String, i32> {
        let mut out = std::collections::HashMap::new();
        for r in self.rules.iter().filter(|r| r.filters.is_empty()) {
            out.entry(r.name.clone()).or_insert(r.id);
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.tags.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Macros;

    #[test]
    fn names_states_and_precedence() {
        let text = "block.10=stone minecraft:dirt mod:thing \\\n grass_block:snowy=false\n\
                    block.11=grass_block\nblock.12=oak_leaves:persistent=true:distance=1,2 %leaves\n\
                    layer.translucent=glass\n";
        let ids = BlockIds::parse(&Properties::parse(text, &Macros::standard()));
        let none = |_: &str| None;
        assert_eq!(ids.id("minecraft:stone", none), Some(10));
        assert_eq!(ids.id("mod:thing", none), Some(10));
        assert_eq!(ids.id("minecraft:glass", none), None);
        let snowy = |s: &'static str| move |k: &str| (k == "snowy").then_some(s);
        // The filtered entry is the more specific one.
        assert_eq!(ids.id("minecraft:grass_block", snowy("false")), Some(10));
        assert_eq!(ids.id("minecraft:grass_block", snowy("true")), Some(11));
        let leaves = |p: &'static str, d: &'static str| {
            move |k: &str| match k {
                "persistent" => Some(p),
                "distance" => Some(d),
                _ => None,
            }
        };
        assert_eq!(
            ids.id("minecraft:oak_leaves", leaves("true", "2")),
            Some(12)
        );
        assert_eq!(ids.id("minecraft:oak_leaves", leaves("true", "3")), None);
        assert_eq!(ids.id("minecraft:oak_leaves", leaves("false", "1")), None);
        assert!(ids.notes.is_empty(), "{:?}", ids.notes);
    }

    #[test]
    fn block_tags_resolve_current_members_with_individual_priority() {
        use std::collections::{HashMap, HashSet};

        let parse = |text| BlockIds::parse(&Properties::parse(text, &Macros::standard()));
        // A tag-only file is still an ID mapping file, even before its members are known.
        assert!(!parse("block.20=%grove").is_empty());
        let declared = parse(
            "block.30=oak_log\n\
            block.20=%grove:axis=y,z:waterlogged=false\n\
            block.31=birch_log:axis=x\n\
            block.21=%demo:grove\nblock.90=%missing\n",
        );
        assert!(declared.notes.is_empty(), "{:?}", declared.notes);
        let mut tags: HashMap<String, HashSet<String>> = HashMap::from([
            (
                "minecraft:grove".into(),
                HashSet::from(["minecraft:oak_log".into(), "minecraft:birch_log".into()]),
            ),
            ("demo:grove".into(), HashSet::from(["demo:stem".into()])),
        ]);
        let ids = declared.with_block_tags(&tags);
        let state = |axis| {
            move |key: &str| match key {
                "axis" => Some(axis),
                "waterlogged" => Some("false"),
                _ => None,
            }
        };
        // Individuals before and after tag declarations win when they match.
        // The unfiltered individual also beats a tag with two matching filters.
        assert_eq!(ids.id("minecraft:oak_log", state("y")), Some(30));
        assert_eq!(ids.id("minecraft:birch_log", state("x")), Some(31));
        assert_eq!(ids.id("minecraft:birch_log", state("z")), Some(20));
        assert_eq!(ids.id("minecraft:birch_log", |_| None), None);
        assert_eq!(
            ids.id("minecraft:birch_log", |k| match k {
                "axis" => Some("y"),
                "waterlogged" => Some("true"),
                _ => None,
            }),
            None
        );
        assert_eq!(ids.id("demo:stem", |_| None), Some(21));
        assert_eq!(ids.id("minecraft:missing", |_| None), None);
        assert_eq!(ids.id("minecraft:stone", |_| None), None);

        // A datapack replacement removes old members as well as adding new ones.
        tags.insert(
            "minecraft:grove".into(),
            HashSet::from(["minecraft:spruce_log".into()]),
        );
        tags.remove("demo:grove");
        let reloaded = ids.with_block_tags(&tags);
        assert_eq!(reloaded.id("minecraft:birch_log", state("y")), None);
        assert_eq!(reloaded.id("minecraft:spruce_log", state("y")), Some(20));
        assert_eq!(reloaded.id("demo:stem", |_| None), None);
        assert_eq!(reloaded.id("minecraft:oak_log", state("y")), Some(30));
        assert_eq!(reloaded.id("minecraft:birch_log", state("x")), Some(31));
        assert_eq!(declared.id("minecraft:spruce_log", state("y")), None);
    }

    #[test]
    fn entity_lines_by_name() {
        let text = "entity.10=zombie minecraft:husk\nentity.11=zombie mod:thing\nblock.5=stone\n";
        let ids = BlockIds::parse_kind(&Properties::parse(text, &Macros::standard()), "entity");
        let plain = ids.plain();
        assert_eq!(
            plain["minecraft:zombie"], 10,
            "the first line naming it wins"
        );
        assert_eq!(plain["minecraft:husk"], 10);
        assert_eq!(plain["mod:thing"], 11);
        assert!(!plain.contains_key("minecraft:stone"));
    }

    #[test]
    fn continuation_across_blank_lines_and_inline_keys_follow_iris() {
        let text = "block.10=stone \\\n\n  dirt\nblock.11=sand block.12=gravel\nblock.13=glass\r\n";
        let ids = BlockIds::from_pack_text(Some(text), &Macros::standard());
        let none = |_: &str| None;
        assert_eq!(ids.id("minecraft:dirt", none), Some(10));
        assert_eq!(ids.id("minecraft:sand", none), Some(11));
        assert_eq!(ids.id("minecraft:gravel", none), Some(12));
        assert_eq!(ids.id("minecraft:glass", none), Some(13));
        // Ordinary continuations are untouched.
        let ids = BlockIds::from_pack_text(Some("block.1=a \\\n b\n"), &Macros::standard());
        assert_eq!(ids.id("minecraft:b", |_| None), Some(1));
    }

    #[test]
    fn packs_without_block_properties_read_the_legacy_ids() {
        let ids = BlockIds::from_pack_text(None, &Macros::standard());
        let none = |_: &str| None;
        assert_eq!(ids.id("minecraft:water", none), Some(9));
        assert_eq!(ids.id("minecraft:lava", none), Some(11));
        assert_eq!(ids.id("minecraft:oak_leaves", none), Some(18));
        assert_eq!(ids.id("minecraft:dark_oak_leaves", none), Some(18));
        assert_eq!(ids.id("minecraft:glass", none), None);
        assert_eq!(ids.id("minecraft:red_stained_glass", none), Some(95));
        assert_eq!(ids.id("minecraft:lime_stained_glass_pane", none), Some(160));
        assert_eq!(ids.id("minecraft:emerald_block", none), Some(-123));
        assert_eq!(ids.id("minecraft:torch", none), Some(50));
        assert_eq!(ids.id("minecraft:tall_seagrass", none), Some(175));
        assert!(ids.notes.is_empty(), "{:?}", ids.notes);
    }
}
