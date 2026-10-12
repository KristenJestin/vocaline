//! Rules per application: what happens to a dictation depends on the window it started in.
//!
//! The first rule whose class matches the window wins; a window that matches none gets the
//! defaults, which are no rule at all.

use crate::vocabulary::Vocabulary;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// What `start --rule` and `press --rule` call it: one word.
    pub name: String,
    /// The window class it applies to, case ignored, `*` standing for any run of characters
    /// (`org.example.*`). Without one the rule is only ever forced by name.
    #[serde(default)]
    pub class: Option<String>,
    /// False: the dictation stays in the event stream and nothing is pasted.
    #[serde(default = "yes")]
    pub paste: bool,
    /// Press Enter once after the paste: send in a chat, run in a terminal.
    #[serde(default)]
    pub enter: bool,
    /// Drop the period that ends the pasted text. The phrases of the event stream keep theirs.
    #[serde(default)]
    pub drop_period: bool,
    /// Lines of the vocabulary file's format, added to the common vocabulary for the
    /// dictations this rule governs; on the same heard words, the rule wins.
    #[serde(default)]
    pub vocabulary: Vec<String>,
}

fn yes() -> bool {
    true
}

impl Rule {
    fn matches(&self, class: &str) -> bool {
        self.class
            .as_deref()
            .is_some_and(|pattern| glob(pattern, class))
    }

    /// The rule's own vocabulary, and the lines of it that cannot be read.
    pub fn vocabulary(&self) -> (Vocabulary, Vec<String>) {
        let (vocabulary, problems) = Vocabulary::parse(&self.vocabulary.join("\n"));
        let problems = problems
            .into_iter()
            .map(|problem| format!("rule {}: {problem}", self.name))
            .collect();
        (vocabulary, problems)
    }

    /// The text as this rule wants it pasted.
    pub fn finish(&self, text: &str) -> String {
        if self.drop_period {
            drop_final_period(text).to_string()
        } else {
            text.to_string()
        }
    }
}

/// The first rule whose class matches the window `class`; none when the window is unknown.
pub fn select<'a>(rules: &'a [Rule], class: Option<&str>) -> Option<&'a Rule> {
    let class = class?;
    rules.iter().find(|rule| rule.matches(class))
}

pub fn named<'a>(rules: &'a [Rule], name: &str) -> Option<&'a Rule> {
    rules.iter().find(|rule| rule.name == name)
}

/// What a configuration file must satisfy for the daemon to trust it, so a mistake is told at
/// startup, not at the first dictation it would have changed.
pub fn check(rules: &[Rule]) -> Result<(), String> {
    for (at, rule) in rules.iter().enumerate() {
        if rule.name.is_empty() || rule.name.contains(char::is_whitespace) {
            return Err(format!(
                "rule {}: a name is one word, as `start --rule <name>` reads it",
                at + 1
            ));
        }
        if rules[..at].iter().any(|known| known.name == rule.name) {
            return Err(format!("rule {}: the name is used twice", rule.name));
        }
        if let Some(problem) = rule.vocabulary().1.into_iter().next() {
            return Err(problem);
        }
    }
    Ok(())
}

/// The text without the period that ends it. Three dots are an ellipsis, not a period.
fn drop_final_period(text: &str) -> &str {
    match text.strip_suffix('.') {
        Some(rest) if !rest.ends_with('.') => rest,
        _ => text,
    }
}

fn glob(pattern: &str, text: &str) -> bool {
    let (pattern, text) = (pattern.to_lowercase(), text.to_lowercase());
    let mut parts: Vec<&str> = pattern.split('*').collect();
    let last = parts.pop().unwrap_or_default();
    if parts.is_empty() {
        return text == last;
    }
    let first = parts.remove(0);
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    for part in parts {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

#[cfg(test)]
mod tests;
