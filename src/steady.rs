//! Steadies the partial phrase. The engine reads the open utterance again at every step, and two
//! close readings of the same words can win in turn (`simple, avec` then `simple avec`, step
//! after step): drawn on screen, the phrase blinks. A word already shown changes only when two
//! readings in a row agree on the change; the end of the phrase follows the engine at once.

use std::ops::Range;

/// The partial of one utterance, as shown, and the engine's last reading of it.
#[derive(Debug, Default)]
pub struct Steady {
    shown: Vec<String>,
    last_reading: Vec<String>,
}

impl Steady {
    /// The phrase to show for the engine's new reading; none when it does not change.
    pub fn read(&mut self, text: &str) -> Option<String> {
        let reading: Vec<String> = text.split_whitespace().map(String::from).collect();
        let confirmed = changes(&self.shown, &self.last_reading);
        let mut next = Vec::with_capacity(reading.len());
        let mut at = 0;
        for change in changes(&self.shown, &reading) {
            next.extend_from_slice(&self.shown[at..change.old.start]);
            let at_the_end = change.old.end == self.shown.len();
            let read_before = confirmed.iter().any(|before| {
                before.old == change.old
                    && self.last_reading[before.new.clone()] == reading[change.new.clone()]
            });
            if at_the_end || read_before {
                next.extend_from_slice(&reading[change.new]);
            } else {
                next.extend_from_slice(&self.shown[change.old.clone()]);
            }
            at = change.old.end;
        }
        next.extend_from_slice(&self.shown[at..]);
        self.last_reading = reading;
        if next == self.shown {
            return None;
        }
        self.shown = next;
        Some(self.shown.join(" "))
    }
}

/// A stretch where `new` differs from `old`.
#[derive(Debug, PartialEq)]
struct Change {
    old: Range<usize>,
    new: Range<usize>,
}

/// Where `new` differs from `old`, word for word, around their longest common subsequence.
fn changes(old: &[String], new: &[String]) -> Vec<Change> {
    // common[i][j]: the length of the longest common subsequence of old[i..] and new[j..].
    let mut common = vec![vec![0usize; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            common[i][j] = if old[i] == new[j] {
                common[i + 1][j + 1] + 1
            } else {
                common[i + 1][j].max(common[i][j + 1])
            };
        }
    }
    let mut found = Vec::new();
    let (mut i, mut j) = (0, 0);
    let mut open: Option<(usize, usize)> = None;
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && old[i] == new[j] {
            if let Some((oi, oj)) = open.take() {
                found.push(Change {
                    old: oi..i,
                    new: oj..j,
                });
            }
            i += 1;
            j += 1;
            continue;
        }
        open.get_or_insert((i, j));
        if j == new.len() || (i < old.len() && common[i + 1][j] >= common[i][j + 1]) {
            i += 1;
        } else {
            j += 1;
        }
    }
    if let Some((oi, oj)) = open {
        found.push(Change {
            old: oi..i,
            new: oj..j,
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(steady: &mut Steady, readings: &[&str]) -> Vec<Option<String>> {
        readings.iter().map(|r| steady.read(r)).collect()
    }

    #[test]
    fn the_phrase_grows_as_the_engine_reads_it() {
        let mut steady = Steady::default();
        assert_eq!(
            shown(
                &mut steady,
                &["Bonjour", "Bonjour, je", "Bonjour, je parle."]
            ),
            [
                Some("Bonjour".into()),
                Some("Bonjour, je".into()),
                Some("Bonjour, je parle.".into())
            ]
        );
    }

    #[test]
    fn two_readings_that_alternate_do_not_make_the_phrase_blink() {
        let mut steady = Steady::default();
        assert_eq!(
            shown(
                &mut steady,
                &[
                    "une personne de manière simple, avec",
                    "une personne de manière simple avec son",
                    "une personne de manière simple, avec son nom",
                    "une personne de manière simple avec son nom et",
                ]
            ),
            [
                Some("une personne de manière simple, avec".into()),
                Some("une personne de manière simple, avec son".into()),
                Some("une personne de manière simple, avec son nom".into()),
                Some("une personne de manière simple, avec son nom et".into()),
            ]
        );
    }

    #[test]
    fn a_change_read_twice_in_a_row_is_shown() {
        let mut steady = Steady::default();
        assert_eq!(
            shown(
                &mut steady,
                &[
                    "les nôtres entre elles",
                    "les notes entre elles quand",
                    "les notes entre elles quand elles",
                ]
            ),
            [
                Some("les nôtres entre elles".into()),
                Some("les nôtres entre elles quand".into()),
                Some("les notes entre elles quand elles".into()),
            ]
        );
    }

    #[test]
    fn the_last_word_follows_the_engine_at_once() {
        let mut steady = Steady::default();
        assert_eq!(
            shown(
                &mut steady,
                &[
                    "on y retrouve les idées, les rends.",
                    "on y retrouve les idées, les rendez-vous"
                ]
            ),
            [
                Some("on y retrouve les idées, les rends.".into()),
                Some("on y retrouve les idées, les rendez-vous".into()),
            ]
        );
    }

    #[test]
    fn a_reading_that_drops_the_start_once_is_not_shown() {
        let mut steady = Steady::default();
        assert_eq!(
            shown(
                &mut steady,
                &[
                    "Il s'accompagne d'un autre projet. On y retrouve",
                    "On y retrouve les",
                    "Il s'accompagne d'un autre projet. On y retrouve les idées",
                ]
            ),
            [
                Some("Il s'accompagne d'un autre projet. On y retrouve".into()),
                Some("Il s'accompagne d'un autre projet. On y retrouve les".into()),
                Some("Il s'accompagne d'un autre projet. On y retrouve les idées".into()),
            ]
        );
    }

    #[test]
    fn the_same_reading_again_shows_nothing_new() {
        let mut steady = Steady::default();
        assert_eq!(
            shown(&mut steady, &["Bonjour, je", "Bonjour, je"]),
            [Some("Bonjour, je".into()), None]
        );
    }
}
