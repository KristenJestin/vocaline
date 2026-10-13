use super::*;
use jiff::civil::date;

fn folder(name: &str) -> PathBuf {
    let folder = std::env::temp_dir().join(format!("vocaline-stats-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    folder
}

fn entry(day: Date, app: &str, words: u32, audio_ms: u64, paste_ms: Option<u64>) -> Entry {
    Entry {
        day,
        dictation: Dictation {
            app: app.into(),
            words,
            audio_ms,
            paste_ms,
            cpu_fallback: false,
        },
    }
}

const TODAY: Date = date(2026, 10, 12);

/// A week of dictation in two invented applications, an old one, and one with no window.
fn sample() -> Vec<Entry> {
    vec![
        entry(
            date(2026, 9, 1),
            "org.example.Editor",
            50,
            30_000,
            Some(900),
        ),
        entry(
            date(2026, 10, 5),
            "org.example.Editor",
            10,
            6_000,
            Some(400),
        ),
        entry(
            date(2026, 10, 6),
            "org.example.Editor",
            20,
            12_000,
            Some(300),
        ),
        entry(date(2026, 10, 6), "org.example.Chat", 5, 3_000, Some(200)),
        entry(TODAY, "org.example.Chat", 40, 24_000, Some(500)),
        entry(TODAY, "org.example.Editor", 60, 36_000, Some(100)),
        entry(TODAY, UNKNOWN, 12, 6_000, None),
    ]
}

#[test]
fn words_are_what_has_a_letter_or_a_digit() {
    assert_eq!(count_words("Bonjour."), 1);
    assert_eq!(count_words("Ça va ?"), 2);
    assert_eq!(count_words("  l'été   est-ce que 3 fois ! "), 5);
    assert_eq!(count_words(""), 0);
    assert_eq!(count_words(" ... ? "), 0);
}

#[test]
fn the_dictations_are_counted_by_period() {
    let report = report(&sample(), TODAY);
    assert_eq!(report.date, TODAY);
    assert_eq!(report.today.totals.dictations, 3);
    assert_eq!(report.today.totals.words, 112);
    assert_eq!(report.today.totals.audio_ms, 66_000);
    // Today and the six days before it: the 5th and the first of September are only in all time.
    assert_eq!(report.last_7_days.totals.dictations, 5);
    assert_eq!(report.last_7_days.totals.words, 137);
    assert_eq!(report.all_time.totals.dictations, 7);
    assert_eq!(report.all_time.totals.words, 197);
    assert_eq!(report.all_time.totals.audio_ms, 117_000);
}

#[test]
fn the_seven_days_start_six_days_before_today() {
    let report = report(
        &[
            entry(date(2026, 10, 5), "a", 1, 1_000, None),
            entry(date(2026, 10, 6), "a", 1, 1_000, None),
            // Written by a clock that was ahead: it counts for all time only.
            entry(date(2026, 10, 13), "a", 1, 1_000, None),
        ],
        TODAY,
    );
    assert_eq!(report.last_7_days.totals.dictations, 1);
    assert_eq!(report.today.totals.dictations, 0);
    assert_eq!(report.all_time.totals.dictations, 3);
}

#[test]
fn the_dictations_are_grouped_by_application_the_most_dictated_first() {
    let report = report(&sample(), TODAY);
    let apps: Vec<(&str, u32, u64)> = report
        .last_7_days
        .by_application
        .iter()
        .map(|a| (a.app.as_str(), a.totals.dictations, a.totals.words))
        .collect();
    assert_eq!(
        apps,
        [
            ("org.example.Editor", 2, 80),
            ("org.example.Chat", 2, 45),
            (UNKNOWN, 1, 12)
        ]
    );
    assert_eq!(report.today.by_application.len(), 3);
    assert_eq!(report.all_time.by_application[0].totals.words, 140);
}

#[test]
fn the_dictations_are_grouped_by_day_oldest_first_without_the_empty_days() {
    let report = report(&sample(), TODAY);
    let days: Vec<(Date, u32, u64)> = report
        .last_7_days
        .by_day
        .iter()
        .map(|d| (d.day, d.totals.dictations, d.totals.words))
        .collect();
    assert_eq!(days, [(date(2026, 10, 6), 2, 25), (TODAY, 3, 112)]);
    assert_eq!(report.all_time.by_day.len(), 4);
    assert_eq!(report.all_time.by_day[0].day, date(2026, 9, 1));
}

#[test]
fn the_time_to_paste_is_the_median_and_the_max_of_the_dictations_that_were_pasted() {
    let report = report(&sample(), TODAY);
    // Today: 500 and 100; the one with no paste is left out.
    assert_eq!(
        report.today.totals.paste,
        Some(Latency {
            count: 2,
            median_ms: 300,
            max_ms: 500
        })
    );
    assert_eq!(
        report.all_time.totals.paste,
        Some(Latency {
            count: 6,
            median_ms: 350,
            max_ms: 900
        })
    );
    let unpasted = super::report(&[entry(TODAY, UNKNOWN, 3, 1_000, None)], TODAY);
    assert_eq!(unpasted.today.totals.paste, None);
}

#[test]
fn an_empty_store_reports_nothing_at_all() {
    let report = report(&[], TODAY);
    assert_eq!(report.all_time.totals.dictations, 0);
    assert_eq!(report.all_time.totals.paste, None);
    assert!(report.all_time.by_application.is_empty());
    assert!(report.all_time.by_day.is_empty());
}

#[test]
fn the_text_shows_each_application_with_its_words_and_minutes() {
    let text = report(&sample(), TODAY).text();
    assert!(text.contains("today (2026-10-12)"), "{text}");
    assert!(text.contains("last 7 days"), "{text}");
    assert!(text.contains("all time"), "{text}");
    // Today: 3 dictations, 112 words, 66 s of audio.
    assert!(text.contains("3 dictations, 112 words, 1.1 min"), "{text}");
    let editor = text
        .lines()
        .find(|line| line.contains("org.example.Editor") && line.contains("60 words"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(editor.contains("1 dictation,"), "{editor}");
    assert!(editor.contains("0.6 min"), "{editor}");
    assert!(
        text.contains("stop to paste: median 300 ms, max 500 ms"),
        "{text}"
    );
    assert!(text.contains("2026-10-06"), "{text}");
}

#[test]
fn a_period_with_no_dictation_says_so() {
    let text = report(&[], TODAY).text();
    assert_eq!(text.matches("no dictation").count(), 3, "{text}");
}

#[test]
fn a_dictation_is_appended_as_one_line_of_counts_and_durations() {
    let store = Store::new(folder("append").join("vocaline").join("stats.jsonl"));
    let dictation = Dictation {
        app: "org.example.Editor".into(),
        words: 12,
        audio_ms: 4_500,
        paste_ms: Some(320),
        cpu_fallback: false,
    };
    store.record(&dictation).unwrap();
    store
        .record(&Dictation {
            paste_ms: None,
            ..dictation.clone()
        })
        .unwrap();

    let (entries, skipped) = store.read().unwrap();
    assert_eq!(skipped, 0);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].dictation, dictation);
    assert_eq!(entries[1].dictation.paste_ms, None);
    let written = std::fs::read_to_string(&store.path).unwrap();
    assert_eq!(written.lines().count(), 2);
    // The day is the local one, written as a date.
    assert!(written.starts_with("{\"day\":\"20"), "{written}");
}

#[test]
fn a_store_that_does_not_exist_yet_is_empty() {
    let store = Store::new(folder("missing").join("stats.jsonl"));
    assert_eq!(store.read().unwrap(), (Vec::new(), 0));
}

#[test]
fn a_line_that_cannot_be_read_is_skipped_and_counted() {
    let path = folder("torn").join("stats.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        "{\"day\":\"2026-10-12\",\"app\":\"a\",\"words\":2,\"audio_ms\":1000}\n\
         {\"day\":\"2026-10-12\",\"app\":\"a\",\"wor\n\
         \n\
         {\"day\":\"2026-10-12\",\"app\":\"b\",\"words\":3,\"audio_ms\":2000,\"paste_ms\":150}\n",
    )
    .unwrap();
    let (entries, skipped) = Store::new(path).read().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].dictation.paste_ms, Some(150));
    assert_eq!(skipped, 1);
}

#[test]
fn the_json_has_the_three_periods_with_their_groups() {
    let path = folder("json").join("stats.jsonl");
    let store = Store::new(path);
    store
        .record(&Dictation {
            app: "org.example.Editor".into(),
            words: 7,
            audio_ms: 3_000,
            paste_ms: Some(250),
            cpu_fallback: false,
        })
        .unwrap();
    let mut out = Vec::new();
    let skipped = print(&store, jiff::Zoned::now().date(), true, &mut out).unwrap();
    assert_eq!(skipped, 0);
    let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
    for period in ["today", "last_7_days", "all_time"] {
        assert_eq!(json[period]["dictations"], 1, "{json}");
        assert_eq!(json[period]["words"], 7, "{json}");
        assert_eq!(json[period]["audio_ms"], 3_000, "{json}");
        assert_eq!(json[period]["paste"]["median_ms"], 250, "{json}");
        assert_eq!(
            json[period]["by_application"][0]["app"],
            "org.example.Editor"
        );
        assert_eq!(json[period]["by_day"][0]["words"], 7);
    }
}

#[test]
fn the_text_is_printed_from_the_store_without_the_daemon() {
    let store = Store::new(folder("text").join("stats.jsonl"));
    let mut out = Vec::new();
    print(&store, TODAY, false, &mut out).unwrap();
    assert!(String::from_utf8(out).unwrap().contains("no dictation"));
}

// The GPU is shared: how often it had no memory for a dictation tells whether to change the setup.
#[test]
fn the_dictations_decoded_again_on_the_cpu_are_counted() {
    let mut entries = sample();
    entries[5].dictation.cpu_fallback = true;
    entries[2].dictation.cpu_fallback = true;
    let counted = report(&entries, TODAY);
    assert_eq!(counted.today.totals.cpu_fallbacks, 1);
    assert_eq!(counted.last_7_days.totals.cpu_fallbacks, 2);
    assert_eq!(counted.today.by_application[0].totals.cpu_fallbacks, 1);
    let text = counted.text();
    assert!(
        text.contains("decoded on the CPU, the GPU being full: 1"),
        "{text}"
    );
    assert!(
        text.contains("decoded on the CPU, the GPU being full: 2"),
        "{text}"
    );
    let quiet = report(&sample(), TODAY).text();
    assert!(!quiet.contains("on the CPU"), "{quiet}");
}

#[test]
fn a_dictation_on_the_gpu_writes_no_fallback_field() {
    let store = Store::new(folder("fallback").join("stats.jsonl"));
    let dictation = Dictation {
        app: "org.example.Editor".into(),
        words: 3,
        audio_ms: 1_000,
        paste_ms: None,
        cpu_fallback: false,
    };
    store.record(&dictation).unwrap();
    store
        .record(&Dictation {
            cpu_fallback: true,
            ..dictation.clone()
        })
        .unwrap();
    let written = std::fs::read_to_string(&store.path).unwrap();
    let lines: Vec<&str> = written.lines().collect();
    assert!(!lines[0].contains("cpu_fallback"), "{written}");
    assert!(lines[1].ends_with(",\"cpu_fallback\":true}"), "{written}");
    let (entries, _) = store.read().unwrap();
    assert!(!entries[0].dictation.cpu_fallback);
    assert!(entries[1].dictation.cpu_fallback);
}
