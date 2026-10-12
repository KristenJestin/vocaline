//! The vocabulary of one's own: words the model gets wrong, and how they must be written.
//!
//! A plain file, one entry per line, `heard -> wanted`; empty lines and lines starting with `#`
//! are left alone, and when two lines hear the same words the later one wins (that is what
//! `teach` relies on: it only appends).

use std::path::Path;

/// What separates the heard words from the wanted ones in the file.
const ARROW: &str = "->";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Vocabulary {
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    /// The heard words, folded (see `fold`).
    heard: Vec<Vec<char>>,
    wanted: String,
}

impl Vocabulary {
    /// Reads the file at `path`; a missing file is an empty vocabulary. The lines that cannot be
    /// read are left out and named in the second part, so one typo does not silence the rest.
    pub fn load(path: &Path) -> (Vocabulary, Vec<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => Vocabulary::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vocabulary::default(), vec![]),
            Err(e) => (
                Vocabulary::default(),
                vec![format!("{}: {e}", path.display())],
            ),
        }
    }

    pub fn parse(text: &str) -> (Vocabulary, Vec<String>) {
        let mut entries: Vec<Entry> = Vec::new();
        let mut problems = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let entry = line.split_once(ARROW).and_then(|(heard, wanted)| {
                let heard: Vec<Vec<char>> = heard.split_whitespace().map(fold).collect();
                let wanted = wanted.trim();
                (!heard.is_empty() && !wanted.is_empty()).then(|| Entry {
                    heard,
                    wanted: wanted.to_string(),
                })
            });
            match entry {
                Some(entry) => {
                    entries.retain(|known| known.heard != entry.heard);
                    entries.push(entry);
                }
                None => problems.push(format!(
                    "vocabulary line {}: expected `heard words {ARROW} wanted words`",
                    number + 1
                )),
            }
        }
        // The longest first: "foo bar" must win over "foo" where both start.
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.heard.len()));
        (Vocabulary { entries }, problems)
    }

    /// This vocabulary with `other` on top: on the same heard words, `other` wins.
    pub fn with(mut self, other: &Vocabulary) -> Vocabulary {
        for entry in &other.entries {
            self.entries.retain(|known| known.heard != entry.heard);
            self.entries.push(entry.clone());
        }
        self.entries
            .sort_by_key(|entry| std::cmp::Reverse(entry.heard.len()));
        self
    }

    /// The text with every heard entry replaced by the way it must be written.
    pub fn correct(&self, text: &str) -> String {
        if self.entries.is_empty() {
            return text.to_string();
        }
        let chars: Vec<char> = text.chars().collect();
        // Folded one character for one, so a position in one is a position in the other.
        let folded: Vec<char> = chars.iter().copied().map(fold_char).collect();
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        while at < chars.len() {
            let starts_word =
                chars[at].is_alphanumeric() && (at == 0 || !chars[at - 1].is_alphanumeric());
            let found = starts_word
                .then(|| {
                    self.entries
                        .iter()
                        .find_map(|entry| Some((matched(&entry.heard, &folded, at)?, entry)))
                })
                .flatten();
            match found {
                Some((end, entry)) => {
                    out.push_str(&entry.wanted);
                    at = end;
                }
                None => {
                    out.push(chars[at]);
                    at += 1;
                }
            }
        }
        out
    }
}

/// Where the words `heard` end in `text` when they start at `at`, with only whitespace between
/// them and a boundary after the last one.
pub(crate) fn matched(heard: &[Vec<char>], text: &[char], at: usize) -> Option<usize> {
    let mut end = at;
    for (n, word) in heard.iter().enumerate() {
        if n > 0 {
            let gap = text[end..].iter().take_while(|c| c.is_whitespace()).count();
            if gap == 0 {
                return None;
            }
            end += gap;
        }
        if text.get(end..end + word.len())? != word.as_slice() {
            return None;
        }
        end += word.len();
    }
    match text.get(end) {
        Some(c) if c.is_alphanumeric() => None,
        _ => Some(end),
    }
}

/// Lowercase, without accents: what two spellings of a word have in common.
pub(crate) fn fold(word: &str) -> Vec<char> {
    word.chars().map(fold_char).collect()
}

/// One character for one, whatever the case or the accent (`œ` and `ß` stay: they are two
/// letters, not an accent).
fn fold_char(c: char) -> char {
    let c = c.to_lowercase().next().unwrap_or(c);
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
        'ç' | 'ć' | 'č' => 'c',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
        'ñ' | 'ń' | 'ň' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ō' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' => 'u',
        'ý' | 'ÿ' => 'y',
        'š' | 'ś' => 's',
        'ž' | 'ź' | 'ż' => 'z',
        c => c,
    }
}

/// Adds the entry "heard -> wanted" at the end of the file at `path`, which is created (with its
/// folder) when it is missing.
pub fn teach(path: &Path, heard: &str, wanted: &str) -> Result<(), String> {
    let (heard, wanted) = (heard.trim(), wanted.trim());
    if heard.is_empty() || wanted.is_empty() {
        return Err("both the heard words and the wanted words are needed".into());
    }
    if heard.contains(ARROW) {
        return Err(format!("the heard words cannot hold `{ARROW}`"));
    }
    if heard.contains('\n') || wanted.contains('\n') {
        return Err("an entry is one line".into());
    }
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder).map_err(fail)?;
    }
    let before = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(fail(e)),
    };
    let separator = if before.is_empty() || before.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(fail)?;
    use std::io::Write;
    writeln!(file, "{separator}{heard} {ARROW} {wanted}").map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocabulary(lines: &str) -> Vocabulary {
        let (vocabulary, problems) = Vocabulary::parse(lines);
        assert!(problems.is_empty(), "{problems:?}");
        vocabulary
    }

    fn folder(name: &str) -> std::path::PathBuf {
        let folder =
            std::env::temp_dir().join(format!("vocaline-vocabulary-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        folder
    }

    #[test]
    fn a_heard_entry_is_written_as_wanted() {
        let v = vocabulary("foo bar -> Foobar");
        assert_eq!(v.correct("ouvre foo bar demain"), "ouvre Foobar demain");
    }

    #[test]
    fn case_is_ignored_when_matching() {
        let v = vocabulary("foo bar -> Foobar");
        assert_eq!(v.correct("Foo BAR est là"), "Foobar est là");
    }

    #[test]
    fn accents_are_ignored_on_both_sides() {
        let v = vocabulary("élodie -> Elodie\nzoe -> Zoé");
        assert_eq!(v.correct("avec Elodie et Zoé"), "avec Elodie et Zoé");
        assert_eq!(v.correct("avec ÉLODIE et zoe"), "avec Elodie et Zoé");
    }

    #[test]
    fn a_word_that_merely_contains_an_entry_is_not_changed() {
        let v = vocabulary("bar -> Bar");
        assert_eq!(v.correct("un barbare au bar"), "un barbare au Bar");
        assert_eq!(v.correct("le sous-bar"), "le sous-Bar");
        assert_eq!(v.correct("barre"), "barre");
        assert_eq!(v.correct("crabar"), "crabar");
    }

    #[test]
    fn punctuation_and_apostrophes_are_boundaries() {
        let v = vocabulary("zorba -> Zorba");
        assert_eq!(v.correct("c'est zorba, non ?"), "c'est Zorba, non ?");
        assert_eq!(v.correct("l'zorba."), "l'Zorba.");
    }

    #[test]
    fn several_words_match_across_any_run_of_spaces() {
        let v = vocabulary("foo bar baz -> FooBarBaz");
        assert_eq!(v.correct("un foo  bar\tbaz ici"), "un FooBarBaz ici");
        assert_eq!(v.correct("foo bar"), "foo bar");
    }

    #[test]
    fn a_vocabulary_on_top_of_another_adds_to_it_and_wins_on_the_same_words() {
        let common = vocabulary("foo bar -> Foobar\nélodie -> Elodie");
        let own = vocabulary("baz -> Baz\nfoo bar -> FOOBAR");
        let both = common.with(&own);
        assert_eq!(
            both.correct("foo bar, baz et élodie"),
            "FOOBAR, Baz et Elodie"
        );
    }

    #[test]
    fn the_longest_entry_wins_where_two_start_together() {
        let v = vocabulary("foo -> F\nfoo bar -> Foobar");
        assert_eq!(v.correct("foo bar et foo"), "Foobar et F");
    }

    #[test]
    fn a_replacement_is_not_corrected_again() {
        let v = vocabulary("a -> b\nb -> c");
        assert_eq!(v.correct("a b"), "b c");
    }

    #[test]
    fn the_later_line_wins_for_the_same_words() {
        let v = vocabulary("foo bar -> Foobar\nFOO  Bar -> FooBar");
        assert_eq!(v.correct("foo bar"), "FooBar");
    }

    #[test]
    fn comments_and_empty_lines_are_left_alone() {
        let v = vocabulary("# my words\n\nfoo -> Foo\n   \n");
        assert_eq!(v.correct("foo"), "Foo");
    }

    #[test]
    fn an_empty_vocabulary_changes_nothing() {
        assert_eq!(Vocabulary::default().correct("foo bar"), "foo bar");
    }

    #[test]
    fn a_line_that_cannot_be_read_is_named_and_the_others_still_apply() {
        let (v, problems) =
            Vocabulary::parse("foo -> Foo\nno arrow here\nbar ->\n-> Baz\nbar -> Bar");
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("line 2"), "{problems:?}");
        assert_eq!(v.correct("foo bar"), "Foo Bar");
    }

    #[test]
    fn a_missing_file_is_an_empty_vocabulary() {
        let (v, problems) = Vocabulary::load(&folder("missing").join("vocabulary.txt"));
        assert_eq!(v, Vocabulary::default());
        assert!(problems.is_empty());
    }

    #[test]
    fn teach_creates_the_file_and_adds_one_line_per_entry() {
        let path = folder("teach").join("config").join("vocabulary.txt");
        teach(&path, "foo bar", "Foobar").unwrap();
        teach(&path, "baz", "Baz").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "foo bar -> Foobar\nbaz -> Baz\n"
        );
        let (v, _) = Vocabulary::load(&path);
        assert_eq!(v.correct("foo bar baz"), "Foobar Baz");
    }

    #[test]
    fn teach_keeps_what_the_file_already_holds() {
        let path = folder("keep").join("vocabulary.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# mine\nfoo -> Foo").unwrap();
        teach(&path, "bar", "Bar").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\nfoo -> Foo\nbar -> Bar\n"
        );
    }

    #[test]
    fn teach_refuses_what_the_file_could_not_read_back() {
        let path = folder("refuse").join("vocabulary.txt");
        assert!(teach(&path, "", "Foo").is_err());
        assert!(teach(&path, "  ", "Foo").is_err());
        assert!(teach(&path, "foo", " ").is_err());
        assert!(teach(&path, "foo -> bar", "Foo").is_err());
        assert!(teach(&path, "foo\nbar", "Foo").is_err());
        assert!(teach(&path, "foo", "Foo\nBar").is_err());
        assert!(!path.exists());
    }
}
