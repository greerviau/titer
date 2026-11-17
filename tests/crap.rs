mod common;

use common::{Fixture, assert_schema_valid, fixture_python, stdout_json};

const HEAD: &str = r#"def grade(x):
    if x > 90:
        return "A"
    elif x > 80:
        return "B"
    return "C"


def unused(y):
    return y
"#;

fn repo(extra: &str) -> Fixture {
    let repo = Fixture::new();
    repo.write(
        "titer.toml",
        &format!(
            "[[runner]]\nkind = \"pytest\"\ncommand = [\"{}\", \"-m\", \"pytest\"]\n{extra}",
            fixture_python()
        ),
    )
    .write(
        "app.py",
        "def grade(x):\n    return \"C\"\n\n\ndef unused(y):\n    return 0\n",
    );
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "change"]);
    repo.write("app.py", HEAD);
    repo
}

// Hand calculation. grade has two decision points (if, elif), so complexity 3. Coverage
// marks lines 1, 2, 3, and 6 of its six lines run: 4/6. CRAP = 3^2 * (1 - 4/6)^3 + 3 =
// 9/27 + 3 = 3.33. unused has complexity 1 and no covered line: 1 * 1 + 1 = 2.
const LCOV: &str = "SF:/somewhere/else/app.py\nDA:1,1\nDA:2,5\nDA:3,2\nDA:4,0\nDA:5,0\nDA:6,3\nDA:9,0\nDA:10,0\nend_of_record\n";
const COBERTURA: &str = r#"<?xml version="1.0" ?>
<coverage><sources><source>/ci/repo</source></sources><packages><package name="."><classes>
<class name="app.py" filename="app.py"><lines>
<line number="1" hits="1"/><line number="2" hits="5"/><line number="3" hits="2"/><line number="4" hits="0"/>
<line number="5" hits="0"/><line number="6" hits="3"/><line number="9" hits="0"/><line number="10" hits="0"/>
</lines></class></classes></package></packages></coverage>
"#;
const COVERAGE_PY: &str = r#"{"meta": {"format": 3}, "files": {"app.py": {"executed_lines": [1, 2, 3, 6], "missing_lines": [4, 5, 9, 10], "excluded_lines": []}}}"#;

fn scores(report: &serde_json::Value) -> Vec<(String, u64, f64, f64, bool)> {
    report["crap"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["function"].as_str().unwrap().to_string(),
                c["complexity"].as_u64().unwrap(),
                c["coverage"].as_f64().unwrap(),
                c["score"].as_f64().unwrap(),
                c["flagged"].as_bool().unwrap(),
            )
        })
        .collect()
}

#[test]
fn each_coverage_format_gives_the_hand_calculated_scores() {
    for (name, text) in [
        ("coverage.info", LCOV),
        ("coverage.xml", COBERTURA),
        ("coverage.json", COVERAGE_PY),
    ] {
        let repo = repo("\n[crap]\nthreshold = 3\n");
        let path = repo.scratch().join(name);
        std::fs::write(&path, text).unwrap();

        let output = repo.titer(&[
            "check",
            "--json",
            "--only",
            "crap",
            "--coverage",
            path.to_str().unwrap(),
        ]);

        let report = stdout_json(&output);
        assert_schema_valid(&report);
        assert_eq!(output.status.code(), Some(0), "{name}: {report:#}");
        assert_eq!(
            scores(&report),
            vec![
                ("grade".to_string(), 3, 0.667, 3.33, true),
                ("unused".to_string(), 1, 0.0, 2.0, false),
            ],
            "{name}"
        );
    }
}

#[test]
fn the_coverage_command_runs_in_a_worktree_and_fail_on_crap_fails_the_gate() {
    let command = format!("printf '%s' '{COVERAGE_PY}' > coverage.json");
    let repo = repo(&format!(
        "\n[gate]\nfail_on_crap = true\n\n[crap]\nthreshold = 3\ncoverage_command = [\"sh\", \"-c\", {}]\ncoverage_file = \"coverage.json\"\n",
        serde_json::to_string(&command).unwrap()
    ));
    let before = repo.checkout_state();

    let output = repo.titer(&["check", "--json", "--only", "crap"]);

    let report = stdout_json(&output);
    assert_eq!(output.status.code(), Some(1), "{report:#}");
    assert!(
        report["gate"]["reasons"][0]
            .as_str()
            .unwrap()
            .contains("fail_on_crap")
    );
    assert_eq!(scores(&report)[0].3, 3.33);
    assert_eq!(repo.checkout_state(), before);
}

#[test]
fn without_coverage_crap_reports_complexity_alone() {
    let repo = repo("");

    let output = repo.titer(&["check", "--json", "--only", "crap"]);

    let report = stdout_json(&output);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(report["crap"][0]["complexity"], 3);
    assert_eq!(report["crap"][0]["score"], serde_json::Value::Null);
    assert_eq!(report["crap"][0]["coverage"], serde_json::Value::Null);
}
