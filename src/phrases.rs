//! Text steps applied to what the engine heard, without any model: the filler words are dropped
//! from each phrase.

use crate::vocabulary::{fold, matched};

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
            let end = starts_word
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
    fn a_phrase_without_filler_is_left_untouched() {
        let text = "  Bonjour  tout le monde.  ";
        assert_eq!(Fillers::default().drop(text), text);
    }
}
