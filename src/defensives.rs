//! A deliberately small set of major defensive and raid cooldown casts.
//! These are spell identities, not assumed durations or cooldown availability.
//! Verify retail spell IDs when updating this list; names come from WCL master data.
//! Reference: https://github.com/Tercioo/Open-Raid-Library/blob/8a6e6bdb2b6df4e628f24ff54b76d6d00239ea04/ThingsToMantain_Midnight.lua

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefensiveGroup {
    Personal,
    External,
    Raid,
}

const PERSONAL: &[u64] = &[
    31850, 498, 403876, 642, 86659, 184662, // Paladin
    118038, 184364, 12975, 871, // Warrior
    108416, 104773, // Warlock
    108271, 108270, // Shaman
    122278, 122783, 243435, 115203, // Monk
    186265, 264735, // Hunter
    22812, 61336, // Druid
    48707, 48792, 55233,  // Death Knight
    198589, // Demon Hunter
    110959, 45438, 414658, // Mage
    19236, 47585, // Priest
    31224, 5277, // Rogue
    363916, 374348, // Evoker
];
const EXTERNAL: &[u64] = &[
    1022, 6940, 204018, 633, 116849, 102342, 47788, 33206, 357170,
];
const RAID: &[u64] = &[
    31821, 97462, 114052, 108280, 98008, 115310, 388615, 740, 51052, 196718, 414660, 64843, 62618,
    359816, 363534, 374227,
];

pub fn classify(id: u64) -> Option<DefensiveGroup> {
    if PERSONAL.contains(&id) {
        Some(DefensiveGroup::Personal)
    } else if EXTERNAL.contains(&id) {
        Some(DefensiveGroup::External)
    } else if RAID.contains(&id) {
        Some(DefensiveGroup::Raid)
    } else {
        None
    }
}

pub fn filter_expression() -> String {
    let ids: Vec<_> = PERSONAL
        .iter()
        .chain(EXTERNAL)
        .chain(RAID)
        .map(u64::to_string)
        .collect();
    format!("ability.id IN ({})", ids.join(","))
}
