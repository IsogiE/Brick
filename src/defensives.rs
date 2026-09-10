//! Spell identities used by the review timeline; no inferred durations or readiness.
//! Midnight spell IDs checked against OpenRaid on 2026-09-10:
//! https://github.com/Tercioo/Open-Raid-Library/blob/main/ThingsToMantain_Midnight.lua
//! Healing/DR/utility grouping is Brick's editable review policy, not a WCL tag.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DefensiveGroup {
    Personal,
    External,
    Healing,
    DamageReduction,
    Utility,
}

impl DefensiveGroup {
    pub const ALL: [Self; 5] = [
        Self::Personal,
        Self::External,
        Self::Healing,
        Self::DamageReduction,
        Self::Utility,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Personal => "Personal defensives",
            Self::External => "Externals",
            Self::Healing => "Healing CDs",
            Self::DamageReduction => "Damage reduction",
            Self::Utility => "Buffs / utility",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Observation {
    CastOrBuff,
    #[default]
    Cast,
    Buff,
}
impl Observation {
    pub const ALL: [Self; 3] = [Self::CastOrBuff, Self::Cast, Self::Buff];
    pub fn label(self) -> &'static str {
        match self {
            Self::CastOrBuff => "Casts + fallback buffs",
            Self::Cast => "Casts only",
            Self::Buff => "Buff applications",
        }
    }
    pub fn accepts(self, kind: &str) -> bool {
        matches!(
            (self, kind),
            (Self::CastOrBuff | Self::Cast, "cast") | (Self::CastOrBuff | Self::Buff, "applybuff")
        )
    }
}

#[derive(Clone, Copy)]
pub struct Spell {
    pub id: u64,
    pub name: &'static str,
    pub group: DefensiveGroup,
    pub default_enabled: bool,
}
macro_rules! spells {
    ($($group:ident [$enabled:literal] => [$($id:literal: $name:literal),* $(,)?]),* $(,)?) => {
        pub const SPELLS: &[Spell] = &[$($(Spell { id: $id, name: $name, group: DefensiveGroup::$group, default_enabled: $enabled },)*)*];
    };
}
// Keep the original personal/external set. Add only major raid healing/DR
// gaps by default; optional catalog entries remain available in the editor.
// Celestial Conduit is a 90-second raid cooldown in the Midnight registry.
// Rapture/Stasis and shorter, utility or mixed-purpose abilities are opt-in.
spells! {
    Personal [true] => [
        31850: "Ardent Defender",
        498: "Divine Protection",
        403876: "Divine Protection",
        642: "Divine Shield",
        86659: "Guardian of Ancient Kings",
        184662: "Shield of Vengeance",
        118038: "Die by the Sword",
        184364: "Enraged Regeneration",
        12975: "Last Stand",
        871: "Shield Wall",
        108416: "Dark Pact",
        104773: "Unending Resolve",
        108271: "Astral Shift",
        108270: "Stone Bulwark Totem",
        122278: "Dampen Harm",
        122783: "Diffuse Magic",
        243435: "Fortifying Brew",
        115203: "Fortifying Brew",
        186265: "Aspect of the Turtle",
        264735: "Survival of the Fittest",
        22812: "Barkskin",
        61336: "Survival Instincts",
        48707: "Anti-Magic Shell",
        48792: "Icebound Fortitude",
        55233: "Vampiric Blood",
        198589: "Blur",
        110959: "Greater Invisibility",
        45438: "Ice Block",
        414658: "Ice Cold",
        19236: "Desperate Prayer",
        47585: "Dispersion",
        31224: "Cloak of Shadows",
        5277: "Evasion",
        363916: "Obsidian Scales",
        374348: "Renewing Blaze",
    ],
    Personal [false] => [
        389539: "Sentinel",
        387174: "Eye of Tyr",
        122470: "Touch of Karma",
        115176: "Zen Meditation",
        109304: "Exhilaration",
        102558: "Incarnation: Guardian of Ursoc",
    ],
    External [true] => [
        1022: "Blessing of Protection",
        6940: "Blessing of Sacrifice",
        204018: "Blessing of Spellwarding",
        633: "Lay on Hands",
        116849: "Life Cocoon",
        102342: "Ironbark",
        47788: "Guardian Spirit",
        33206: "Pain Suppression",
        357170: "Time Dilation",
    ],
    External [false] => [
        360827: "Blistering Scales",
    ],
    Healing [true] => [
        114052: "Ascendance",
        108280: "Healing Tide Totem",
        115310: "Revival",
        388615: "Restoral",
        740: "Tranquility",
        64843: "Divine Hymn",
        359816: "Dream Flight",
        363534: "Rewind",
        200183: "Apotheosis",
        33891: "Incarnation: Tree of Life",
        472433: "Evangelism",
        421453: "Ultimate Penitence",
        265202: "Holy Word: Salvation",
        197721: "Flourish",
        322118: "Invoke Yu'lon, the Jade Serpent",
        325197: "Invoke Chi-Ji, the Red Crane",
        370960: "Emerald Communion",
        443028: "Celestial Conduit",
    ],
    Healing [false] => [
        47536: "Rapture",
        372835: "Lightwell",
        120517: "Halo",
        15286: "Vampiric Embrace",
        124974: "Nature's Vigil",
        216331: "Avenging Crusader",
        370537: "Stasis",
    ],
    DamageReduction [true] => [
        31821: "Aura Mastery",
        97462: "Rallying Cry",
        98008: "Spirit Link Totem",
        51052: "Anti-Magic Zone",
        196718: "Darkness",
        414660: "Mass Barrier",
        62618: "Power Word: Barrier",
        374227: "Zephyr",
        271466: "Luminous Barrier",
    ],
    DamageReduction [false] => [
        198838: "Earthen Wall Totem",
    ],
    Utility [false] => [
        10060: "Power Infusion",
        29166: "Innervate",
        16191: "Mana Tide Totem",
        197908: "Mana Tea",
        64901: "Symbol of Hope",
        31884: "Avenging Wrath",
        375576: "Divine Toll",
        391528: "Convoke the Spirits",
        2825: "Bloodlust",
        32182: "Heroism",
        80353: "Time Warp",
        264667: "Primal Rage",
        390386: "Fury of the Aspects",
    ],
}

pub const MAX_OVERRIDES: usize = 128;
const MAX_SPELL_ID: u64 = 9_999_999;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub group: Option<DefensiveGroup>,
    pub observation: Observation,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    #[serde(default)]
    pub overrides: BTreeMap<u64, Rule>,
    #[serde(default)]
    pub hidden_groups: Vec<DefensiveGroup>,
}
impl Preferences {
    pub fn validate(&self) -> Result<(), String> {
        if self.overrides.len() > MAX_OVERRIDES
            || self
                .overrides
                .keys()
                .any(|id| !(1..=MAX_SPELL_ID).contains(id))
            || self.hidden_groups.len() > DefensiveGroup::ALL.len()
        {
            return Err("Use at most 128 spell changes and spell IDs from 1 to 9999999.".into());
        }
        Ok(())
    }
    pub fn parse_id(value: &str) -> Result<u64, String> {
        let value = value.trim();
        if value.is_empty() || value.len() > 7 || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err("Enter a numeric spell ID from 1 to 9999999.".into());
        }
        value
            .parse::<u64>()
            .ok()
            .filter(|id| (1..=MAX_SPELL_ID).contains(id))
            .ok_or_else(|| "Enter a numeric spell ID from 1 to 9999999.".into())
    }
    pub fn rule(&self, id: u64) -> Option<Rule> {
        self.overrides.get(&id).copied().or_else(|| {
            SPELLS
                .iter()
                .find(|spell| spell.id == id)
                .map(|spell| Rule {
                    group: spell.default_enabled.then_some(spell.group),
                    observation: Observation::Cast,
                })
        })
    }
    pub fn classify(&self, id: u64) -> Option<DefensiveGroup> {
        self.rule(id).and_then(|rule| rule.group)
    }
    pub fn visible(&self, group: DefensiveGroup) -> bool {
        !self.hidden_groups.contains(&group)
    }
    pub fn has_enabled_spells(&self, group: DefensiveGroup) -> bool {
        self.overrides
            .values()
            .any(|rule| rule.group == Some(group))
            || SPELLS.iter().any(|spell| {
                spell.default_enabled
                    && spell.group == group
                    && !self.overrides.contains_key(&spell.id)
            })
    }
    pub fn ids(&self) -> Vec<u64> {
        let mut ids: Vec<_> = SPELLS
            .iter()
            .map(|spell| spell.id)
            .chain(self.overrides.keys().copied())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
    pub fn filter_expression(&self) -> Result<String, String> {
        self.validate()?;
        let ids = self.ids();
        let mut clauses = Vec::new();
        for kind in ["cast", "applybuff"] {
            let selected: Vec<_> = ids
                .iter()
                .filter(|id| {
                    self.rule(**id)
                        .is_some_and(|rule| rule.group.is_some() && rule.observation.accepts(kind))
                })
                .map(u64::to_string)
                .collect();
            if !selected.is_empty() {
                clauses.push(format!(
                    "(type = \"{kind}\" AND ability.id IN ({}))",
                    selected.join(",")
                ));
            }
        }
        // A user may hide every spell; never turn an empty selection into All.
        Ok(if clauses.is_empty() {
            "ability.id = 0".into()
        } else {
            clauses.join(" OR ")
        })
    }
    pub fn load(account: &str) -> Result<Self, String> {
        let Some(bytes) = preference_store(account)?
            .load()
            .map_err(|_| "Unlock protected storage to load your cooldown filters.")?
        else {
            return Ok(Self::default());
        };
        if bytes.len() > 32 * 1024 {
            return Err("Saved cooldown filters are invalid.".into());
        }
        let result: Self =
            serde_json::from_slice(&bytes).map_err(|_| "Saved cooldown filters are invalid.")?;
        result.validate()?;
        Ok(result)
    }
    pub fn save(&self, account: &str) -> Result<(), String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| "Couldn't save cooldown filters.")?;
        preference_store(account)?
            .save(&bytes)
            .map_err(|_| "Unlock protected storage to save your cooldown filters.".into())
    }
}
fn preference_store(account: &str) -> Result<crate::credential_store::Store, String> {
    if account.is_empty() || account.len() > 20 || !account.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Sign in to save your cooldown filters.".into());
    }
    crate::credential_store::Store::new(&format!("cooldown-preferences-v1:{account}"))
}
pub fn spell_name(id: u64) -> Option<&'static str> {
    SPELLS
        .iter()
        .find(|spell| spell.id == id)
        .map(|spell| spell.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn midnight_healing_and_damage_reduction_have_separate_rows() {
        let prefs = Preferences::default();
        for id in [200183, 322118, 325197, 33891, 370960, 443028] {
            assert_eq!(prefs.classify(id), Some(DefensiveGroup::Healing));
        }
        for id in [31821, 62618, 98008, 374227] {
            assert_eq!(prefs.classify(id), Some(DefensiveGroup::DamageReduction));
        }
        assert_eq!(prefs.classify(33206), Some(DefensiveGroup::External));
        let ids = prefs.ids();
        assert_eq!(ids.len(), SPELLS.len(), "Registry contains duplicate IDs");
    }
    #[test]
    fn default_query_preserves_the_sparse_baseline_plus_major_raid_cooldowns() {
        let baseline = [
            31850, 498, 403876, 642, 86659, 184662, 118038, 184364, 12975, 871, 108416, 104773,
            108271, 108270, 122278, 122783, 243435, 115203, 186265, 264735, 22812, 61336, 48707,
            48792, 55233, 198589, 110959, 45438, 414658, 19236, 47585, 31224, 5277, 363916, 374348,
            1022, 6940, 204018, 633, 116849, 102342, 47788, 33206, 357170, 31821, 97462, 114052,
            108280, 98008, 115310, 388615, 740, 51052, 196718, 414660, 64843, 62618, 359816,
            363534, 374227,
        ];
        let additions = [
            200183, 33891, 472433, 421453, 265202, 322118, 325197, 370960, 197721, 271466, 443028,
        ];
        let mut expected: Vec<_> = baseline.into_iter().chain(additions).collect();
        expected.sort_unstable();
        let preferences = Preferences::default();
        let actual: Vec<_> = preferences
            .ids()
            .into_iter()
            .filter(|id| preferences.classify(*id).is_some())
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 71);
        let filter = preferences.filter_expression().unwrap();
        assert!(
            !filter.contains("applybuff"),
            "Automatic aura fallbacks can multiply one raid cooldown into one event per target"
        );
        let queried: Vec<u64> = filter
            .split_once('(')
            .unwrap()
            .1
            .rsplit_once(')')
            .unwrap()
            .0
            .split_once('(')
            .unwrap()
            .1
            .trim_end_matches(')')
            .split(',')
            .map(|id| id.parse().unwrap())
            .collect();
        assert_eq!(
            queried, expected,
            "Optional catalog IDs must be absent from WCL queries"
        );
        assert!(!preferences.has_enabled_spells(DefensiveGroup::Utility));
        for id in [
            120517, 372835, 197908, 375576, 31884, 391528, 370537, 47536, 360827, 387174,
        ] {
            assert_eq!(preferences.rule(id).unwrap().group, None);
            assert!(
                spell_name(id).is_some(),
                "Opt-in spells remain discoverable by name"
            );
        }
    }

    #[test]
    fn optional_catalog_spells_require_an_explicit_rule_and_reset_disables_them() {
        let mut preferences = Preferences::default();
        let id = 197908; // Mana Tea is available in the catalog but not tracked by default.
        assert_eq!(preferences.classify(id), None);
        preferences.overrides.insert(
            id,
            Rule {
                group: Some(DefensiveGroup::Utility),
                observation: Observation::Cast,
            },
        );
        assert!(preferences.has_enabled_spells(DefensiveGroup::Utility));
        assert!(preferences.filter_expression().unwrap().contains("197908"));
        assert_eq!(preferences.classify(id), Some(DefensiveGroup::Utility));
        preferences.overrides.remove(&id);
        assert!(!preferences.has_enabled_spells(DefensiveGroup::Utility));
        assert!(!preferences.filter_expression().unwrap().contains("197908"));
    }

    #[test]
    fn custom_rules_are_bounded_and_cannot_inject_wcl_expressions() {
        for input in [
            "0",
            "-1",
            "1 OR true",
            "12) OR 1=1",
            "99999999",
            "１２３",
            "1.2",
        ] {
            assert!(Preferences::parse_id(input).is_err());
        }
        let mut prefs = Preferences::default();
        prefs.overrides.insert(
            1234567,
            Rule {
                group: Some(DefensiveGroup::Healing),
                observation: Observation::Buff,
            },
        );
        prefs.overrides.insert(
            200183,
            Rule {
                group: None,
                observation: Observation::Cast,
            },
        );
        let filter = prefs.filter_expression().unwrap();
        assert!(!filter.contains("200183"));
        assert_eq!(filter.matches("1234567").count(), 1);
        assert!(filter.contains("type = \"applybuff\""));
        prefs.overrides.insert(
            u64::MAX,
            Rule {
                group: None,
                observation: Observation::Cast,
            },
        );
        assert!(prefs.filter_expression().is_err());
    }
    #[test]
    fn settings_roundtrip_and_restore_preserve_defaults() {
        let mut prefs = Preferences::default();
        prefs.overrides.insert(
            200183,
            Rule {
                group: Some(DefensiveGroup::Utility),
                observation: Observation::Cast,
            },
        );
        prefs.hidden_groups.push(DefensiveGroup::Personal);
        let restored: Preferences =
            serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert_eq!(restored, prefs);
        assert!(!restored.visible(DefensiveGroup::Personal));
        prefs.overrides.remove(&200183);
        assert_eq!(prefs.classify(200183), Some(DefensiveGroup::Healing));
        assert!(serde_json::from_str::<Preferences>(
            r#"{"overrides":{"1":{"group":"arbitrary","observation":"cast"}}}"#
        )
        .is_err());
    }
}
