use super::*;

fn rule(name: &str, class: Option<&str>) -> Rule {
    Rule {
        name: name.into(),
        class: class.map(String::from),
        paste: true,
        enter: false,
        drop_period: false,
        vocabulary: Vec::new(),
    }
}

#[test]
fn the_first_rule_that_matches_the_window_wins() {
    let rules = [
        rule("first", Some("foot")),
        rule("second", Some("foot")),
        rule("any", Some("*")),
    ];
    assert_eq!(select(&rules, Some("foot")).unwrap().name, "first");
    assert_eq!(select(&rules, Some("other")).unwrap().name, "any");
}

#[test]
fn a_window_that_matches_no_rule_gets_none() {
    let rules = [rule("terminal", Some("foot"))];
    assert_eq!(select(&rules, Some("org.example.Chat")), None);
    assert_eq!(select(&[], Some("foot")), None);
}

#[test]
fn an_unknown_window_matches_no_rule_not_even_a_wildcard() {
    assert_eq!(select(&[rule("any", Some("*"))], None), None);
}

#[test]
fn a_rule_without_a_class_is_only_forced_by_its_name() {
    let rules = [rule("shell", None), rule("terminal", Some("foot"))];
    assert_eq!(select(&rules, Some("")), None);
    assert_eq!(named(&rules, "shell").unwrap().name, "shell");
    assert_eq!(named(&rules, "nothing"), None);
}

#[test]
fn a_class_is_matched_whole_and_whatever_the_case() {
    let rules = [rule("terminal", Some("Foot"))];
    assert!(select(&rules, Some("foot")).is_some());
    assert!(select(&rules, Some("FOOT")).is_some());
    assert!(select(&rules, Some("footclient")).is_none());
    assert!(select(&rules, Some("a-foot")).is_none());
}

#[test]
fn a_star_stands_for_any_run_of_characters() {
    for (pattern, class, expected) in [
        ("org.example.*", "org.example.Chat", true),
        ("org.example.*", "org.example.", true),
        ("org.example.*", "org.other.Chat", false),
        ("*chat", "org.example.Chat", true),
        ("*.Chat", "org.example.Chat", true),
        ("org.*.chat", "org.example.Chat", true),
        ("org.*.chat", "org.example.Mail", false),
        ("ab*bc", "abc", false),
        ("ab*bc", "abbc", true),
        ("*", "", true),
        ("a*b*c", "a--b--c", true),
        ("a*b*c", "a--c--b", false),
    ] {
        assert_eq!(glob(pattern, class), expected, "{pattern} on {class}");
    }
}

#[test]
fn a_rule_pastes_and_keeps_the_text_unless_it_says_otherwise() {
    let config: Rule = toml::from_str("name = \"plain\"").unwrap();
    assert_eq!(config, rule("plain", None));
    assert_eq!(config.finish("Bonjour."), "Bonjour.");
}

#[test]
fn dropping_the_period_removes_the_one_that_ends_the_text() {
    let mut terminal = rule("terminal", Some("foot"));
    terminal.drop_period = true;
    assert_eq!(terminal.finish("ls -la."), "ls -la");
    assert_eq!(terminal.finish("ls. Puis cd."), "ls. Puis cd");
    assert_eq!(terminal.finish("cd 1.2."), "cd 1.2");
}

#[test]
fn dropping_the_period_leaves_other_endings_alone() {
    let mut terminal = rule("terminal", Some("foot"));
    terminal.drop_period = true;
    for text in ["Quoi ?", "Ça marche !", "Bon...", "Et puis…", "ls", ""] {
        assert_eq!(terminal.finish(text), text);
    }
}

#[test]
fn a_rule_reads_its_vocabulary_in_the_format_of_the_vocabulary_file() {
    let mut chat = rule("chat", Some("org.example.*"));
    chat.vocabulary = vec!["foo bar -> Foobar".into(), "# a comment".into()];
    let (vocabulary, problems) = chat.vocabulary();
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(vocabulary.correct("ouvre foo bar"), "ouvre Foobar");
}

#[test]
fn a_rule_that_is_wrong_is_refused_with_its_name() {
    let mut broken = rule("chat", None);
    broken.vocabulary = vec!["no arrow here".into()];
    let error = check(&[rule("other", None), broken]).unwrap_err();
    assert!(
        error.contains("chat") && error.contains("vocabulary"),
        "{error}"
    );

    let error = check(&[rule("a", None), rule("a", Some("foot"))]).unwrap_err();
    assert!(error.contains("twice"), "{error}");

    for name in ["", "two words"] {
        assert!(check(&[rule(name, None)]).is_err(), "{name:?}");
    }
    assert_eq!(check(&[rule("a", None), rule("b", Some("foot"))]), Ok(()));
}
