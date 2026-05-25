//! The A/B report reads both metrics schemas correctly and computes net the same way.
use knapsack::ab::compare;
use std::io::Write;
use std::path::PathBuf;

fn write_tmp(tag: &str, contents: &str) -> PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let p = std::env::temp_dir().join(format!("knapsack-ab-{}-{}-{}.jsonl", tag, std::process::id(), t));
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(contents.as_bytes()).unwrap();
    p
}

#[test]
fn computes_net_across_both_schemas() {
    // Knapsack schema: per-session, with delta_hits/evicted and ok flag on expand.
    let kn = write_tmp(
        "kn",
        concat!(
            r#"{"t":1,"event":"compress","session":"s1","raw":3440,"shown":31,"saved":3409,"delta_hits":50,"evicted":0}"#, "\n",
            r#"{"t":2,"event":"expand","session":"s1","tokens":120,"ok":true}"#, "\n",
            r#"{"t":3,"event":"compress","session":"s2","raw":2000,"shown":400,"saved":1600,"delta_hits":2,"evicted":1}"#, "\n",
            r#"{"t":4,"event":"expand","session":"s2","tokens":1900,"ok":true}"#, "\n",
            r#"{"t":5,"event":"expand","session":"s2","tokens":0,"ok":false}"#, "\n",
        ),
    );
    // Rucksack schema: sessionless, orig/comp/saved + expand tokens (no ok field).
    let ru = write_tmp(
        "ru",
        concat!(
            r#"{"t":1,"event":"compress","source":"hook","type":"log","orig":3440,"comp":900,"saved":2540,"handles":1}"#, "\n",
            r#"{"t":2,"event":"compress","source":"mcp","type":"code","orig":2000,"comp":700,"saved":1300,"handles":2}"#, "\n",
            r#"{"t":3,"event":"expand","source":"mcp","handle":"rk_x","tokens":300,"partial":false}"#, "\n",
        ),
    );

    let r = compare(&kn, &ru);

    // knapsack net = (3409 - 120) + (1600 - 1900) = 3289 - 300 = 2989
    assert_eq!(r.knapsack_total.net(), 2989);
    assert_eq!(r.knapsack_total.delta_hits, 52);
    assert_eq!(r.knapsack_total.evicted, 1);
    assert_eq!(r.knapsack_total.failed_expands, 1);

    // rucksack net = (2540 + 1300) - 300 = 3540 (sessionless schema parsed via orig/comp)
    assert_eq!(r.rucksack_total.net(), 3540);
    assert_eq!(r.rucksack_total.raw, 5440);

    // two knapsack sessions, best-net first (s1 positive, s2 negative)
    assert_eq!(r.knapsack_sessions.len(), 2);
    assert_eq!(r.knapsack_sessions[0].0, "s1");
    assert!(r.knapsack_sessions[1].1.net() < 0, "s2 over-expanded -> negative net, honestly reported");

    let _ = std::fs::remove_file(&kn);
    let _ = std::fs::remove_file(&ru);
}
