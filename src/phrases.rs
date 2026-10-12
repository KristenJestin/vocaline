//! Text steps applied to what the engine heard, without any model: the filler words are dropped
//! from each phrase, and the phrases a pause cut in two are joined back in the pasted text.

use crate::vocabulary::{Vocabulary, fold, matched};

/// The hesitations the engine writes, in French and in English. Words that carry meaning
/// (`bah`, `ben`, `bon`, `oui`, `yeah`) are not here: whoever wants them gone says so in the
/// configuration.
pub const DEFAULT_FILLERS: [&str; 9] =
    ["euh", "heu", "euhm", "hum", "hmm", "mmh", "uh", "um", "mm"];

/// The filler words, folded the way the vocabulary folds what it hears.
#[derive(Debug, Clone, PartialEq)]
pub struct Fillers {
    words: Vec<Vec<char>>,
}

impl Default for Fillers {
    fn default() -> Fillers {
        Fillers::new(&DEFAULT_FILLERS)
    }
}

impl Fillers {
    /// An empty list drops nothing.
    pub fn new<S: AsRef<str>>(words: &[S]) -> Fillers {
        Fillers {
            words: words
                .iter()
                .map(|word| fold(word.as_ref().trim()))
                .filter(|word| !word.is_empty())
                .collect(),
        }
    }

    /// The phrase without its filler words, nor the punctuation that hangs on them: an ellipsis
    /// goes with the hesitation it trailed, a comma before the next word too. A mark that
    /// closes a phrase (`.`, `!`, `?`) goes back to the word before the filler, and a word that
    /// now starts a sentence takes a capital.
    pub fn drop(&self, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let folded = fold(text);
        let mut out: Vec<char> = Vec::with_capacity(chars.len());
        let mut dropped = false;
        // The next letter starts a sentence because what began it was a filler.
        let mut capital = false;
        let mut at = 0;
        while at < chars.len() {
            let starts_word =
                chars[at].is_alphanumeric() && (at == 0 || !chars[at - 1].is_alphanumeric());
            // A unit after a number (`5 mm`, `2,5 mm`) is not a hesitation.
            let after_number = chars[..at]
                .iter()
                .rev()
                .find(|c| !c.is_whitespace())
                .is_some_and(char::is_ascii_digit);
            let end = (starts_word && !after_number)
                .then(|| {
                    self.words
                        .iter()
                        .find_map(|word| matched(std::slice::from_ref(word), &folded, at))
                })
                .flatten();
            let Some(end) = end else {
                let c = chars[at];
                if capital && !c.is_whitespace() {
                    out.extend(c.to_uppercase());
                    capital = false;
                } else {
                    out.push(c);
                }
                at += 1;
                continue;
            };
            dropped = true;
            let hanging = chars[end..]
                .iter()
                .take_while(|c| HANGING.contains(c))
                .count();
            let marks = &chars[end..end + hanging];
            let ellipsis = marks.contains(&'…') || marks.windows(2).any(|w| w == ['.', '.']);
            let closing = marks
                .iter()
                .rev()
                .find(|c| CLOSING.contains(c))
                .filter(|_| !ellipsis);
            at = end + hanging;
            while chars.get(at).is_some_and(|c| c.is_whitespace()) {
                at += 1;
            }
            while out.last().is_some_and(|c| c.is_whitespace()) {
                out.pop();
            }
            if let Some(&mark) = closing.filter(|_| !out.is_empty()) {
                if out.last().is_some_and(|c| ",;:".contains(*c)) {
                    out.pop();
                }
                if !out.last().is_some_and(|c| CLOSING.contains(c)) {
                    out.push(mark);
                }
            }
            capital = out.is_empty()
                || (out.last().is_some_and(|c| CLOSING.contains(c)) && !ends_in_ellipsis(&out));
            if at < chars.len() && !out.is_empty() {
                out.push(' ');
            }
        }
        if dropped {
            out.into_iter().collect::<String>().trim().to_string()
        } else {
            text.to_string()
        }
    }
}

/// What hangs on a filler: `euh,` `euh...` `Euh.` `euh ?`.
const HANGING: [char; 8] = [',', ';', ':', '.', '…', '!', '?', '-'];

/// The marks that close a phrase.
const CLOSING: [char; 3] = ['.', '!', '?'];

fn ends_in_ellipsis(text: &[char]) -> bool {
    text.ends_with(&['.', '.']) || text.ends_with(&['…'])
}

/// The phrases of a dictation as one text. They are joined by a space, except where the pause cut
/// a sentence: a phrase that ends with an ellipsis (`...` or `…`) or with no closing mark
/// (`.`, `!`, `?`) goes on in the next one, the ellipsis goes and the next phrase starts
/// lowercase. The ellipsis of the last phrase is kept: nothing follows it.
///
/// A capital stays where the lowercase would be wrong: `I`, a word in capitals or with a capital
/// inside (`NASA`, `McDonald`), a word the vocabulary writes with one.
pub fn join(phrases: &[String], vocabulary: &Vocabulary) -> String {
    let mut text = String::new();
    for phrase in phrases.iter().map(|phrase| phrase.trim()) {
        if phrase.is_empty() {
            continue;
        }
        if text.is_empty() {
            text.push_str(phrase);
        } else if let Some(open) = cut(&text) {
            text = open;
            text.push(' ');
            text.push_str(&lowered(phrase, vocabulary));
        } else {
            text.push(' ');
            text.push_str(phrase);
        }
    }
    text
}

/// What closes a phrase and may follow its last mark: `"Bonjour."`, `(Bonjour.)`, `« Bonjour. »`.
const CLOSERS: [char; 7] = ['"', '\'', '»', '”', ')', ']', '’'];

/// The phrase without its ellipsis when a pause cut it, none when it ended.
fn cut(phrase: &str) -> Option<String> {
    let body = phrase.trim_end_matches(|c: char| c.is_whitespace() || CLOSERS.contains(&c));
    let closers = &phrase[body.len()..];
    if body.ends_with("...") || body.ends_with('…') {
        let open = body.trim_end_matches(['.', '…']).trim_end();
        return Some(format!("{open}{}", closers.trim_end()));
    }
    (!body.ends_with(['.', '!', '?'])).then(|| phrase.to_string())
}

/// The phrase with its first letter lowercase, unless that is a capital to keep.
fn lowered(phrase: &str, vocabulary: &Vocabulary) -> String {
    let Some((start, first)) = phrase.char_indices().find(|(_, c)| c.is_alphanumeric()) else {
        return phrase.to_string();
    };
    let word: String = phrase[start..]
        .chars()
        .take_while(|c| c.is_alphanumeric())
        .collect();
    let keep = !first.is_uppercase()
        || word == "I"
        || word.chars().skip(1).any(char::is_uppercase)
        || vocabulary.capitalises(&word);
    if keep {
        return phrase.to_string();
    }
    let mut out = String::with_capacity(phrase.len());
    out.push_str(&phrase[..start]);
    out.extend(first.to_lowercase());
    out.push_str(&phrase[start + first.len_utf8()..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filler_words_are_dropped_with_their_punctuation() {
        let fillers = Fillers::default();
        assert_eq!(
            fillers.drop("Mais surtout sur la partie euh... Vocaline et tout."),
            "Mais surtout sur la partie Vocaline et tout."
        );
        assert_eq!(fillers.drop("donc, euh, on y va"), "donc, on y va");
        assert_eq!(fillers.drop("on y va euh"), "on y va");
        assert_eq!(fillers.drop("on peut tester euh..."), "on peut tester");
        assert_eq!(fillers.drop("Uh, hello euh Um there"), "Hello there");
    }

    #[test]
    fn a_closing_mark_hanging_on_a_last_filler_goes_back_to_the_phrase() {
        let fillers = Fillers::default();
        assert_eq!(fillers.drop("on y va, euh."), "on y va.");
        assert_eq!(fillers.drop("on y va euh ?"), "on y va ?");
        assert_eq!(fillers.drop("Je pense euh. Donc oui"), "Je pense. Donc oui");
    }

    #[test]
    fn a_phrase_of_only_fillers_is_dropped() {
        let fillers = Fillers::default();
        for text in ["Euh.", "Mm.", "Uh.", "Euh... hum, hmm.", "  "] {
            assert_eq!(fillers.drop(text).trim(), "", "{text:?}");
        }
    }

    #[test]
    fn a_leading_filler_leaves_a_capital() {
        let fillers = Fillers::default();
        assert_eq!(fillers.drop("Euh, donc on y va."), "Donc on y va.");
        assert_eq!(fillers.drop("Euh donc je suis sur…"), "Donc je suis sur…");
        assert_eq!(fillers.drop("Bonjour. Euh, ça va ?"), "Bonjour. Ça va ?");
        assert_eq!(fillers.drop("euh élodie"), "Élodie");
    }

    #[test]
    fn words_that_carry_meaning_are_kept() {
        let fillers = Fillers::default();
        let text = "Bah oui, ben bon, yeah.";
        assert_eq!(fillers.drop(text), text);
    }

    #[test]
    fn an_empty_filler_list_turns_it_off() {
        let none = Fillers::new::<&str>(&[]);
        let text = "Mais surtout sur la partie euh... Vocaline et tout.";
        assert_eq!(none.drop(text), text);
    }

    #[test]
    fn the_list_is_the_one_given() {
        let own = Fillers::new(&["bah", "Ben"]);
        assert_eq!(own.drop("euh, bah oui ben non"), "euh, oui non");
    }

    #[test]
    fn a_filler_inside_a_word_is_kept() {
        let fillers = Fillers::default();
        let text = "un humain heureux, humble et muet";
        assert_eq!(fillers.drop(text), text);
        assert_eq!(fillers.drop("l'humain euh"), "l'humain");
    }

    #[test]
    fn a_unit_after_a_number_is_kept() {
        let fillers = Fillers::default();
        for text in [
            "un tuyau de 5 mm.",
            "épais de 2,5 mm",
            "il mesure 0.5 mm de long",
            "5mm",
        ] {
            assert_eq!(fillers.drop(text), text);
        }
        // Not after a number: still a hesitation.
        assert_eq!(fillers.drop("il y a euh 5 mm"), "il y a 5 mm");
    }

    #[test]
    fn a_phrase_without_filler_is_left_untouched() {
        let text = "  Bonjour  tout le monde.  ";
        assert_eq!(Fillers::default().drop(text), text);
    }

    fn joined(phrases: &[&str]) -> String {
        join_with(phrases, "")
    }

    fn join_with(phrases: &[&str], vocabulary: &str) -> String {
        let (vocabulary, problems) = Vocabulary::parse(vocabulary);
        assert!(problems.is_empty(), "{problems:?}");
        let phrases: Vec<String> = phrases.iter().map(|phrase| phrase.to_string()).collect();
        join(&phrases, &vocabulary)
    }

    #[test]
    fn a_phrase_cut_by_a_pause_is_joined_lowercase() {
        assert_eq!(
            joined(&["Je voulais dire...", "Que ça marche."]),
            "Je voulais dire que ça marche."
        );
        assert_eq!(
            joined(&["Je voulais dire…", "Que ça marche."]),
            "Je voulais dire que ça marche."
        );
        // No closing mark at all, or only a comma: the sentence is not over.
        assert_eq!(
            joined(&["on peut tester ce qui", "Marche bien."]),
            "on peut tester ce qui marche bien."
        );
        assert_eq!(joined(&["Alors,", "On y va."]), "Alors, on y va.");
        assert_eq!(
            joined(&["Il a dit...", "Euh non.", "Rien."]),
            "Il a dit euh non. Rien."
        );
        assert_eq!(
            joined(&["Je voulais...", "Dire que...", "Ça marche."]),
            "Je voulais dire que ça marche."
        );
        assert_eq!(joined(&["J'ai dit...", "À demain."]), "J'ai dit à demain.");
    }

    #[test]
    fn a_phrase_that_ended_is_not_joined() {
        assert_eq!(joined(&["Bonjour.", "Ça va ?"]), "Bonjour. Ça va ?");
        assert_eq!(
            joined(&["Ça va ?", "Oui !", "Super."]),
            "Ça va ? Oui ! Super."
        );
        assert_eq!(
            joined(&["Il dit « bonjour. »", "Puis il part."]),
            "Il dit « bonjour. » Puis il part."
        );
        assert_eq!(joined(&["(Bonjour.)", "Ça va."]), "(Bonjour.) Ça va.");
    }

    #[test]
    fn the_ellipsis_of_the_last_phrase_is_kept_and_a_single_phrase_is_untouched() {
        assert_eq!(
            joined(&["Bonjour.", "Je me demande..."]),
            "Bonjour. Je me demande..."
        );
        assert_eq!(joined(&["Je me demande"]), "Je me demande");
        assert_eq!(joined(&[]), "");
        assert_eq!(joined(&["Bonjour.", "", "  ", "Ça va."]), "Bonjour. Ça va.");
        assert_eq!(
            joined(&["  Je pense... ", " Que oui. "]),
            "Je pense que oui."
        );
    }

    #[test]
    fn a_vocabulary_capital_survives_the_join() {
        let vocabulary = "foo bar -> Foobar\nzoe -> Zoé";
        assert_eq!(
            join_with(&["J'ouvre...", "Foobar avec Zoé."], vocabulary),
            "J'ouvre Foobar avec Zoé."
        );
        assert_eq!(
            join_with(&["On appelle...", "Zoé demain."], vocabulary),
            "On appelle Zoé demain."
        );
        // Without the entry, the capital is only the start of a sentence.
        assert_eq!(
            join_with(&["J'ouvre...", "Foobar marche."], ""),
            "J'ouvre foobar marche."
        );
    }

    #[test]
    fn a_word_in_capitals_or_i_keeps_its_capital_in_a_join() {
        assert_eq!(
            joined(&["Ils travaillent à...", "NASA tous les jours."]),
            "Ils travaillent à NASA tous les jours."
        );
        assert_eq!(
            joined(&["Il a vu...", "McDonald hier."]),
            "Il a vu McDonald hier."
        );
        assert_eq!(
            joined(&["And then...", "I think so."]),
            "And then I think so."
        );
        assert_eq!(joined(&["And then...", "I'm sure."]), "And then I'm sure.");
    }
}
