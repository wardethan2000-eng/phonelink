//! RCS reactions and replies, as one bubble shows them.
//!
//! Google Messages gives each message its reactions and the id of the
//! message it answers; `messages::Messages` keeps them by uid and builds
//! these for the open chat.

use super::gm::GmReaction;

/// The reactions Google Messages offers (its fixed emoji types).
pub const QUICK_REACTIONS: [&str; 7] = ["👍", "😍", "😂", "😮", "😥", "😠", "👎"];

/// What one bubble shows from Google Messages.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Extras {
    pub msg_id: String,
    pub reactions: Vec<Chip>,
    pub quote: Option<Quote>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Chip {
    pub emoji: String,
    pub count: u32,
    pub mine: bool,
    /// "You, Mom" for the tooltip.
    pub who: String,
}

/// The message a reply answers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Quote {
    /// "You", the sender's name, or "" when unknown.
    pub author: String,
    /// Its words, "Picture", or "" when it isn't loaded.
    pub text: String,
    /// The quoted text's bubble, to jump to.
    pub uid: Option<i64>,
}

pub fn chip(r: &GmReaction, name_of: &dyn Fn(&str) -> String) -> Chip {
    let mut who: Vec<String> = Vec::new();
    if r.mine {
        who.push("You".into());
    }
    who.extend(r.by.iter().map(|k| name_of(k)));
    Chip { emoji: r.emoji.clone(), count: r.count, mine: r.mine, who: who.join(", ") }
}

/// The user clicked `emoji` on a message: what to ask Google Messages
/// ("add", "remove" or "switch" from another emoji of mine), with the
/// reactions changed to show it right away.
pub fn toggle(reactions: &mut Vec<GmReaction>, emoji: &str) -> &'static str {
    let same = reactions.iter().any(|r| r.mine && r.emoji == emoji);
    let other = reactions.iter().any(|r| r.mine && r.emoji != emoji);
    for r in reactions.iter_mut().filter(|r| r.mine) {
        r.mine = false;
        r.count = r.count.saturating_sub(1);
    }
    reactions.retain(|r| r.count > 0);
    if same {
        return "remove";
    }
    match reactions.iter_mut().find(|r| r.emoji == emoji) {
        Some(r) => {
            r.mine = true;
            r.count += 1;
        }
        None => reactions.push(GmReaction { emoji: emoji.into(), count: 1, mine: true, by: Vec::new() }),
    }
    if other { "switch" } else { "add" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggling_adds_switches_and_removes() {
        let mut r = vec![GmReaction { emoji: "😂".into(), count: 1, mine: false, by: vec!["k".into()] }];
        let view = |r: &[GmReaction]| r.iter().map(|r| (r.emoji.clone(), r.count, r.mine)).collect::<Vec<_>>();
        assert_eq!(toggle(&mut r, "👍"), "add");
        assert_eq!(view(&r), [("😂".to_string(), 1, false), ("👍".to_string(), 1, true)]);
        assert_eq!(toggle(&mut r, "😂"), "switch");
        assert_eq!(view(&r), [("😂".to_string(), 2, true)]);
        assert_eq!(toggle(&mut r, "😂"), "remove");
        assert_eq!(view(&r), [("😂".to_string(), 1, false)]);
    }

    #[test]
    fn chips_name_who_reacted() {
        let r = GmReaction { emoji: "👍".into(), count: 2, mine: true, by: vec!["3165551111".into()] };
        let name = |k: &str| if k == "3165551111" { "Mom".to_string() } else { k.to_string() };
        assert_eq!(chip(&r, &name), Chip { emoji: "👍".into(), count: 2, mine: true, who: "You, Mom".into() });
    }
}
