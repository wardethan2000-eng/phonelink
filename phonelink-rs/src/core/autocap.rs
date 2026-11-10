//! Automatic capitals while typing a message, as phone keyboards do: the
//! first letter of a sentence, the word "I" (also in "I'm", "I'll"), and
//! names: the user's contacts and a short built-in list (days, months,
//! holidays, a few places and brands). Pure rules here; the compose bar
//! applies them (`ui::thread_view`).

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// What to capitalize after one typed character.
#[derive(Debug, PartialEq, Eq)]
pub enum Autocap {
    /// The character just typed (it starts a sentence).
    Typed,
    /// The lone "i" just before it (the word ended).
    LoneI,
}

/// Which capitals to make (Settings > General > Typing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutocapRules {
    pub sentences: bool,
    /// "I", contacts' names, days, months, holidays, places and brands.
    pub proper_nouns: bool,
}

impl AutocapRules {
    pub const OFF: Self = Self { sentences: false, proper_nouns: false };

    pub fn allows(self, fix: &Autocap) -> bool {
        match fix {
            Autocap::Typed => self.sentences,
            Autocap::LoneI => self.proper_nouns,
        }
    }
}

/// Abbreviations whose full stop does not end a sentence.
const ABBREVIATIONS: &[&str] = &["e.g.", "i.e.", "vs.", "mr.", "mrs.", "ms.", "dr.", "st."];

/// `before` is the text before the cursor, `typed` the character typed there.
pub fn autocap(before: &str, typed: char) -> Option<Autocap> {
    if typed.is_lowercase() {
        return starts_sentence(before).then_some(Autocap::Typed);
    }
    if ends_word(typed) && lone_i_at_end(before) {
        return Some(Autocap::LoneI);
    }
    None
}

/// Whether typing `c` ends the word before it.
pub fn ends_word(c: char) -> bool {
    c.is_whitespace() || "'’,.!?;:)\"”".contains(c)
}

/// Whether a letter typed after `before` begins a sentence: the start of the
/// message, or a space after `.`, `!`, `?` or `…` (closing quotes and
/// brackets allowed in between).
fn starts_sentence(before: &str) -> bool {
    let trimmed = before.trim_end();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.len() == before.len() {
        return false; // still inside a word
    }
    let core = trimmed.trim_end_matches(|c| "\"'”’)]".contains(c));
    if !core.ends_with(['.', '!', '?', '…']) {
        return false;
    }
    let last_word = trimmed.split_whitespace().next_back().unwrap_or("").to_lowercase();
    !ABBREVIATIONS.contains(&last_word.as_str())
}

/// `before` ends with the one-letter word "i".
fn lone_i_at_end(before: &str) -> bool {
    let Some(rest) = before.strip_suffix('i') else {
        return false;
    };
    match rest.chars().next_back() {
        None => true,
        Some(c) => c.is_whitespace() || "\"“(‘'".contains(c),
    }
}

// ── names ──────────────────────────────────────────────────────────

/// Always capitalized (when names are on). Not "May", "Sat", "Sun", "Wed",
/// "Mar", "Turkey" or "China": ordinary words too often.
const BUILT_IN: &[&str] = &[
    // Days and months.
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
    "Mon",
    "Tue",
    "Tues",
    "Thu",
    "Thur",
    "Thurs",
    "Fri",
    "January",
    "February",
    "March",
    "April",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
    "Jan",
    "Feb",
    "Apr",
    "Aug",
    "Sep",
    "Sept",
    "Oct",
    "Nov",
    "Dec",
    // Holidays.
    "Christmas",
    "Xmas",
    "Easter",
    "Thanksgiving",
    "Halloween",
    "Hanukkah",
    "Kwanzaa",
    "Ramadan",
    "Diwali",
    "Valentine",
    "Valentine's",
    "Passover",
    // Places and peoples.
    "America",
    "American",
    "Americans",
    "Canada",
    "Canadian",
    "Mexico",
    "Mexican",
    "England",
    "English",
    "Britain",
    "British",
    "Scotland",
    "Ireland",
    "Irish",
    "France",
    "French",
    "Germany",
    "German",
    "Italy",
    "Italian",
    "Spain",
    "Spanish",
    "Europe",
    "European",
    "Japan",
    "Japanese",
    "Chinese",
    "Korea",
    "Korean",
    "India",
    "Australia",
    "Africa",
    "African",
    "Asia",
    "Asian",
    "Russia",
    "Russian",
    "Ukraine",
    "Israel",
    "Brazil",
    "Hawaii",
    "Alaska",
    "California",
    "Nevada",
    "Oregon",
    "Washington",
    "Idaho",
    "Utah",
    "Arizona",
    "Colorado",
    "Texas",
    "Florida",
    "Vegas",
    "Seattle",
    "Portland",
    "Chicago",
    "Boston",
    "Denver",
    "Phoenix",
    "Dallas",
    "Houston",
    "Atlanta",
    "Disneyland",
    "Disney",
    // Brands people text about.
    "iPhone",
    "iPad",
    "iPhones",
    "iPads",
    "iMessage",
    "AirPods",
    "MacBook",
    "Android",
    "Samsung",
    "Google",
    "YouTube",
    "Facebook",
    "Instagram",
    "TikTok",
    "Snapchat",
    "WhatsApp",
    "Netflix",
    "Hulu",
    "Spotify",
    "Amazon",
    "Uber",
    "Lyft",
    "Venmo",
    "PayPal",
    "Zelle",
    "Costco",
    "Walmart",
    "Starbucks",
    "McDonald's",
    "Chipotle",
    "DoorDash",
    "Xbox",
    "PlayStation",
    "Nintendo",
    "Tesla",
];

/// Words that are never names here even when a contact has them ("Mom",
/// "Pizza Hut"), on top of the spelling dictionary's ordinary words.
const NEVER: &[&str] = &[
    "mom",
    "mommy",
    "momma",
    "mama",
    "mum",
    "mummy",
    "dad",
    "daddy",
    "papa",
    "pa",
    "ma",
    "pop",
    "pops",
    "nana",
    "nan",
    "grandma",
    "grandpa",
    "granny",
    "gramps",
    "grammy",
    "grandad",
    "granddad",
    "grandmother",
    "grandfather",
    "aunt",
    "auntie",
    "aunty",
    "uncle",
    "cousin",
    "sis",
    "sissy",
    "bro",
    "brother",
    "sister",
    "son",
    "daughter",
    "wife",
    "hubby",
    "husband",
    "babe",
    "baby",
    "bae",
    "boo",
    "honey",
    "hun",
    "love",
    "boss",
    "coach",
    "doc",
    "doctor",
    "dr",
    "home",
    "work",
    "office",
    "cell",
    "mobile",
    "phone",
    "main",
    "new",
    "old",
    "me",
    "my",
    "the",
    "and",
    "of",
    "at",
    "from",
    "to",
    "for",
    "in",
    "on",
    "best",
    "friend",
    "bestie",
    "buddy",
    "bud",
    "pal",
    "dude",
    "man",
    "girl",
    "boy",
    "big",
    "little",
    "lil",
    "jr",
    "sr",
    "will",
    "mark",
    "may",
    "bill",
    "grace",
    "hope",
    "rose",
    "faith",
    "joy",
    "art",
    "frank",
    "jack",
    "max",
    "ray",
    "rich",
    "rob",
    "pat",
    "sandy",
    "sunny",
    "summer",
    "autumn",
    "winter",
    "dawn",
    "eve",
    "holly",
    "ivy",
    "lily",
    "olive",
    "pearl",
    "ruby",
    "violet",
    "iris",
    "hazel",
    "heather",
    "jade",
    "amber",
    "crystal",
    "penny",
    "page",
    "chase",
    "hunter",
    "mason",
    "grant",
    "dean",
    "drew",
    "guy",
    "nick",
    "miles",
    "lane",
    "brook",
    "dale",
    "glen",
    "cliff",
    "clay",
    "rocky",
    "rusty",
    "skip",
    "chip",
    "buck",
    "duke",
    "earl",
    "king",
    "major",
    "sky",
    "storm",
    "rain",
    "river",
    "stone",
    "young",
    "price",
    "rice",
    "bell",
    "bush",
    "hill",
    "wood",
    "woods",
    "field",
    "fields",
];

/// First names the dictionary also lists as rare ordinary words ("john",
/// "jimmy" a lock, "graham" a cracker): names when a contact has them.
const FIRST_NAMES: &[&str] = &[
    "john",
    "josh",
    "jimmy",
    "billy",
    "bobby",
    "charlie",
    "patty",
    "molly",
    "tom",
    "martin",
    "peter",
    "pete",
    "graham",
    "nelson",
    "veronica",
    "angelica",
    "gabby",
    "pascal",
    "sally",
    "terry",
    "jerry",
    "barry",
    "larry",
    "harry",
    "gary",
    "kelly",
    "shelly",
    "abby",
    "jenny",
    "betty",
    "dolly",
    "polly",
    "johnny",
    "jimmie",
    "ted",
    "teddy",
    "eddie",
    "freddie",
    "willy",
    "willie",
    "bobbie",
    "bonnie",
    "carol",
    "christian",
    "don",
    "doug",
];

/// The spelling dictionary's ordinary (lowercase) words, if the system has
/// one. Without it, contacts' names are not used (only the built-in list).
pub fn dictionary_words() -> Option<&'static HashSet<String>> {
    static WORDS: OnceLock<Option<HashSet<String>>> = OnceLock::new();
    WORDS
        .get_or_init(|| {
            const PATHS: &[&str] = &[
                "/usr/share/hunspell/en_US.dic",
                "/usr/share/myspell/en_US.dic",
                "/usr/share/hunspell/en_GB.dic",
                "/usr/share/myspell/dicts/en_US.dic",
                "/usr/share/dict/words",
            ];
            let text = PATHS.iter().find_map(|p| std::fs::read_to_string(p).ok())?;
            let words = lowercase_entries(&text);
            log::info!("capitals: {} ordinary words from the spelling dictionary", words.len());
            Some(words)
        })
        .as_ref()
}

/// Entries of a hunspell `.dic` (or a plain word list) that start lowercase.
fn lowercase_entries(text: &str) -> HashSet<String> {
    text.lines()
        .map(|l| l.split('/').next().unwrap_or("").trim())
        .filter(|w| w.chars().next().is_some_and(char::is_lowercase))
        .map(str::to_string)
        .collect()
}

/// Lowercase word → how it is written.
#[derive(Debug, Default, Clone)]
pub struct Names {
    proper: HashMap<String, String>,
}

impl Names {
    /// The built-in list plus each word of the contacts' names that is a
    /// name: starts with a capital, has small letters ("JOHN" is shouting),
    /// and is not an ordinary word. `ordinary` is the dictionary's words;
    /// `None` leaves contacts out.
    pub fn new<'a>(contacts: impl IntoIterator<Item = &'a str>, ordinary: Option<&HashSet<String>>) -> Self {
        let mut proper: HashMap<String, String> = HashMap::new();
        if let Some(ordinary) = ordinary {
            for name in contacts {
                for word in name.split_whitespace() {
                    let word = word.trim_matches(|c: char| !c.is_alphanumeric());
                    let lower = word.to_lowercase();
                    let looks_like_name = word.chars().count() >= 2
                        && word.chars().next().is_some_and(char::is_uppercase)
                        && word.chars().any(char::is_lowercase)
                        && word.chars().all(|c| c.is_alphabetic() || "'’-".contains(c));
                    // "minutes", "lies": the dictionary lists only "minute", "lie".
                    let is_ordinary = ordinary.contains(&lower)
                        || lower.strip_suffix('s').is_some_and(|base| ordinary.contains(base));
                    let is_ordinary = is_ordinary && !FIRST_NAMES.contains(&lower.as_str());
                    if looks_like_name && !is_ordinary && !NEVER.contains(&lower.as_str()) {
                        proper.entry(lower).or_insert_with(|| word.to_string());
                    }
                }
            }
        }
        for w in BUILT_IN {
            proper.insert(w.to_lowercase(), (*w).to_string());
        }
        // Plurals ("Mondays", "iPhones"), only of names of four letters or
        // more and never when the plural is a word itself ("Thu" + s).
        let plurals: Vec<(String, String)> = proper
            .iter()
            .filter(|(k, p)| k.chars().count() >= 4 && !p.ends_with(['s', '\'', '’']))
            .map(|(k, p)| (format!("{k}s"), format!("{p}s")))
            .filter(|(k, _)| !ordinary.is_some_and(|o| o.contains(k)) && !NEVER.contains(&k.as_str()))
            .collect();
        for (k, p) in plurals {
            proper.entry(k).or_insert(p);
        }
        Self { proper }
    }

    /// The word that ends `before` (a word-ending character is being typed
    /// after it), if it is a name typed in small letters: (its first
    /// character's index, the name as written).
    pub fn fix(&self, before: &str) -> Option<(usize, String)> {
        let chars: Vec<char> = before.chars().collect();
        let mut start = chars.len();
        while start > 0 && (chars[start - 1].is_alphabetic() || "'’-".contains(chars[start - 1])) {
            start -= 1;
        }
        // Quotes and dashes around it are not part of it.
        while start < chars.len() && "'’-".contains(chars[start]) {
            start += 1;
        }
        if start == chars.len() {
            return None;
        }
        // Not inside an address, a username or a file name.
        if start > 0 && (chars[start - 1].is_alphanumeric() || "@/._#:\\&=".contains(chars[start - 1])) {
            return None;
        }
        let word: String = chars[start..].iter().collect();
        let proper = self.lookup(&word)?;
        let titled = {
            let mut c = word.chars();
            c.next().is_some_and(char::is_uppercase) && c.all(|c| !c.is_uppercase())
        };
        let typed_small = word.chars().all(|c| !c.is_uppercase());
        // "zelda" → "Zelda"; "Iphone" (a sentence start) → "iPhone".
        let wanted = typed_small
            || (titled && proper.chars().skip(1).any(char::is_uppercase))
            || (titled && proper.chars().next().is_some_and(char::is_lowercase));
        (wanted && proper != word && proper.chars().count() == word.chars().count()).then_some((start, proper))
    }

    fn lookup(&self, word: &str) -> Option<String> {
        self.proper.get(&word.to_lowercase()).cloned()
    }

    pub fn len(&self) -> usize {
        self.proper.len()
    }

    pub fn is_empty(&self) -> bool {
        self.proper.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_letter_of_a_message() {
        assert_eq!(autocap("", 'h'), Some(Autocap::Typed));
        assert_eq!(autocap("  ", 'h'), Some(Autocap::Typed));
        assert_eq!(autocap("", 'H'), None);
        assert_eq!(autocap("", '5'), None);
    }

    #[test]
    fn after_a_sentence() {
        assert_eq!(autocap("Done. ", 's'), Some(Autocap::Typed));
        assert_eq!(autocap("Really?  ", 'w'), Some(Autocap::Typed));
        assert_eq!(autocap("Yes! ", 'o'), Some(Autocap::Typed));
        assert_eq!(autocap("Wait… ", 'o'), Some(Autocap::Typed));
        assert_eq!(autocap("He said \"no.\" ", 't'), Some(Autocap::Typed));
        assert_eq!(autocap("(See above.) ", 't'), Some(Autocap::Typed));
    }

    #[test]
    fn not_inside_a_sentence() {
        assert_eq!(autocap("Hello ", 't'), None);
        assert_eq!(autocap("Done.", 's'), None); // no space yet: "3.5", "a.m"
        assert_eq!(autocap("Hello, ", 't'), None);
        assert_eq!(autocap("Hello th", 'e'), None);
        assert_eq!(autocap("Bring snacks, e.g. ", 'c'), None);
        assert_eq!(autocap("Ask Dr. ", 's'), None);
    }

    #[test]
    fn the_word_i() {
        assert_eq!(autocap("i", ' '), Some(Autocap::LoneI));
        assert_eq!(autocap("Yes i", ' '), Some(Autocap::LoneI));
        assert_eq!(autocap("Yes i", '\''), Some(Autocap::LoneI)); // i'm, i'll, i've, i'd
        assert_eq!(autocap("Yes i", '’'), Some(Autocap::LoneI));
        assert_eq!(autocap("so do i", '?'), Some(Autocap::LoneI));
        assert_eq!(autocap("so do i", '.'), Some(Autocap::LoneI));
        assert_eq!(autocap("\"i", ' '), Some(Autocap::LoneI));
    }

    fn names() -> Names {
        let ordinary: HashSet<String> = ["will", "mark", "pizza", "hut", "grace", "march", "minute", "john"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        Names::new(
            [
                "Zelda Smith",
                "Will Turner",
                "Pizza Hut",
                "Mom",
                "JOHN DOE",
                "Mary-Jane O'Brien",
                "Dr. Lee (dentist)",
                "Phone Minutes",
                "John Adams",
            ],
            Some(&ordinary),
        )
    }

    #[test]
    fn contact_names_get_capitals() {
        let n = names();
        assert_eq!(n.fix("hi zelda"), Some((3, "Zelda".into())));
        assert_eq!(n.fix("zelda"), Some((0, "Zelda".into())));
        assert_eq!(n.fix("ask smith"), Some((4, "Smith".into())));
        assert_eq!(n.fix("with o'brien"), Some((5, "O'Brien".into())));
        assert_eq!(n.fix("with mary-jane"), Some((5, "Mary-Jane".into())));
        assert_eq!(n.fix("\"zelda"), Some((1, "Zelda".into())));
        assert_eq!(n.fix("lee"), Some((0, "Lee".into())));
    }

    #[test]
    fn ordinary_words_in_contacts_stay_small() {
        let n = names();
        assert_eq!(n.fix("i will"), None); // "Will Turner"
        assert_eq!(n.fix("get pizza"), None); // "Pizza Hut"
        assert_eq!(n.fix("call mom"), None);
        assert_eq!(n.fix("doe"), None); // only written in capitals ("JOHN DOE")
        assert_eq!(n.fix("ask turner"), Some((4, "Turner".into())));
        assert_eq!(n.fix("dentist"), None);
        assert_eq!(n.fix("five minutes"), None); // plural of an ordinary word
    }

    #[test]
    fn first_names_that_are_also_rare_words() {
        let n = names();
        assert_eq!(n.fix("ask john"), Some((4, "John".into())));
        assert_eq!(n.fix("adams"), Some((0, "Adams".into())));
    }

    #[test]
    fn only_whole_words_typed_small() {
        let n = names();
        assert_eq!(n.fix("Zelda"), None);
        assert_eq!(n.fix("ZELDA"), None);
        assert_eq!(n.fix("zeldas"), Some((0, "Zeldas".into())));
        assert_eq!(n.fix("zelda@gmail.com/zelda"), None);
        assert_eq!(n.fix("mail zelda.smith"), None);
        assert_eq!(n.fix("bezelda"), None);
    }

    #[test]
    fn built_in_names() {
        let n = names();
        assert_eq!(n.fix("see you monday"), Some((8, "Monday".into())));
        assert_eq!(n.fix("on mondays"), Some((3, "Mondays".into())));
        assert_eq!(n.fix("in march"), Some((3, "March".into()))); // the list wins over the dictionary
        assert_eq!(n.fix("for christmas"), Some((4, "Christmas".into())));
        assert_eq!(n.fix("my iphone"), Some((3, "iPhone".into())));
        assert_eq!(n.fix("Iphone"), Some((0, "iPhone".into())));
        assert_eq!(n.fix("going to costco"), Some((9, "Costco".into())));
        assert_eq!(n.fix("mcdonald's"), Some((0, "McDonald's".into())));
        assert_eq!(n.fix("it may"), None);
        assert_eq!(n.fix("the sun"), None);
        assert_eq!(n.fix("we sat"), None);
        assert_eq!(n.fix("and thus"), None); // not "Thu" + s
        assert_eq!(n.fix("on thu"), Some((3, "Thu".into())));
    }

    #[test]
    fn no_dictionary_no_contacts() {
        let n = Names::new(["Zelda Smith"], None);
        assert_eq!(n.fix("zelda"), None);
        assert_eq!(n.fix("monday"), Some((0, "Monday".into())));
    }

    #[test]
    fn reads_a_hunspell_dic() {
        let words = lowercase_entries("4\nwill/DS\nWill/M\nzelda-not\nEthan/M\n");
        assert!(words.contains("will"));
        assert!(!words.contains("Will") && !words.contains("Ethan"));
    }

    #[test]
    fn sentences_and_proper_nouns_switch_apart() {
        let only_nouns = AutocapRules { sentences: false, proper_nouns: true };
        assert!(!only_nouns.allows(&Autocap::Typed));
        assert!(only_nouns.allows(&Autocap::LoneI));
        assert!(!AutocapRules::OFF.allows(&Autocap::LoneI));
    }

    #[test]
    fn i_inside_other_words_stays() {
        assert_eq!(autocap("Hi", ' '), None);
        assert_eq!(autocap("wi-fi", ' '), None);
        assert_eq!(autocap("pi", ' '), None);
        assert_eq!(autocap("Yes i", 'n'), None); // "in" still being typed
        assert_eq!(autocap("file.i", ' '), None);
        assert_eq!(autocap("Yes I", ' '), None);
    }
}
