//! Automatic capitals while typing a message, as phone keyboards do: the
//! first letter of a sentence, and the word "I" (also in "I'm", "I'll").
//! Pure rules here; the compose bar applies them (`ui::thread_view`).

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
    pub lone_i: bool,
}

impl AutocapRules {
    pub const OFF: Self = Self { sentences: false, lone_i: false };

    pub fn allows(self, fix: &Autocap) -> bool {
        match fix {
            Autocap::Typed => self.sentences,
            Autocap::LoneI => self.lone_i,
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
    let ends_word = typed.is_whitespace() || "'’,.!?;:)\"”".contains(typed);
    if ends_word && lone_i_at_end(before) {
        return Some(Autocap::LoneI);
    }
    None
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

    #[test]
    fn each_rule_has_its_own_switch() {
        let only_i = AutocapRules { sentences: false, lone_i: true };
        assert!(!only_i.allows(&Autocap::Typed));
        assert!(only_i.allows(&Autocap::LoneI));
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
