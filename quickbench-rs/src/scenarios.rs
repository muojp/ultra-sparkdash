//! Named workloads, and the review cases that carry their own answer keys.

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub struct Scenario {
    pub name: &'static str,
    pub max_tokens: u32,
    pub prompt: &'static str,
    pub json_schema: bool,
}

/// These recipes swap speculative engines that change *rank* by workload — one prompt shape
/// measures one corner and reads like a verdict — so the sweep runs a set.
pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "chat",
        max_tokens: 256,
        prompt: "A colleague asks why their HTTP service got slower after moving to a new host. \
                 Answer conversationally in a short paragraph, then list two things to check first.",
        json_schema: false,
    },
    Scenario {
        name: "code",
        max_tokens: 512,
        prompt: "Write a Python function `merge_ranges(ranges)` that merges overlapping closed \
                 integer intervals and returns them sorted. Handle empty input and single \
                 intervals. Include a short docstring and three assert-based examples.",
        json_schema: false,
    },
    Scenario {
        name: "essay",
        max_tokens: 900,
        prompt: "Write a continuous essay of about 700 words on why distributed systems favour \
                 idempotent operations. No lists, no headings — prose only.",
        json_schema: false,
    },
    Scenario {
        // A reasoning model spends hundreds of tokens before it writes anything; this has to leave
        // room for both the trace and the answer.
        name: "review",
        max_tokens: 1600,
        prompt: "",
        json_schema: false,
    },
    Scenario {
        name: "structured",
        max_tokens: 400,
        prompt: "Describe the Unix operating system: its name, the year it first appeared, and \
                 three languages it has been written in.",
        json_schema: true,
    },
];

pub fn scenario(name: &str) -> Option<&'static Scenario> {
    SCENARIOS.iter().find(|s| s.name == name)
}

#[derive(Debug, Deserialize, Clone)]
pub struct Case {
    pub id: String,
    pub language: String,
    pub defect: String,
    pub expected: String,
    pub code: String,
    #[serde(default)]
    pub signals: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CaseFile {
    cases: Vec<Case>,
}

pub fn load_cases(explicit: Option<&Path>) -> Vec<Case> {
    let path: PathBuf = match explicit {
        Some(p) => p.to_path_buf(),
        None => std::env::var("REVIEW_CASES")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_cases_path()),
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str::<CaseFile>(&s)
            .map(|f| f.cases)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn default_cases_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    // Installed next to the other tools: <root>/bin/<exe>, cases at <root>/bench/review_cases.
    exe.parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("bench/review_cases/cases.json"))
        .unwrap_or_else(|| PathBuf::from("bench/review_cases/cases.json"))
}

/// Everything about a case except its language and code is an answer key: the id names the defect,
/// `defect` is its category, `expected` is the finding. None of it may reach the model, and neither
/// may a prompt listing the categories — that turns "find the problem" into multiple choice.
pub fn review_prompt(case: &Case) -> String {
    format!(
        "Review this {} file as you would in a pull request.\n\n\
         If something is wrong, say what it is and why it matters, in two or three sentences, \
         then give the corrected code. If nothing is wrong, say so.\n\n```\n{}```\n",
        case.language, case.code
    )
}

/// Which answer-key field, if any, shows through in what we are about to send.
pub fn case_leak(prompt: &str, case: &Case) -> Option<String> {
    let low = prompt.to_lowercase();
    let benign = format!("{} {}", case.code, case.language).to_lowercase();
    for word in case.id.split('-') {
        if word.len() > 3 && low.contains(&word.to_lowercase()) && !benign.contains(&word.to_lowercase()) {
            return Some(format!("id word {word:?}"));
        }
    }
    if low.contains(&case.defect.to_lowercase()) {
        return Some("defect".into());
    }
    let head: String = case.expected.to_lowercase().split(". ").next().unwrap_or("").to_string();
    if head.len() > 24 && low.contains(&head) {
        return Some("expected phrase".into());
    }
    None
}

/// Context filler sized in tokens and unique per seed, plus the scenario's instruction. The filler
/// keeps prefill comparable across scenarios; the instruction is what makes the decode differ.
pub fn build_prompt(approx_tokens: u32, seed: u64, scen: &Scenario, cases: &[Case]) -> String {
    if scen.name == "review" {
        if cases.is_empty() {
            return "Review this file.".into();
        }
        let case = &cases[(seed as usize) % cases.len()];
        let p = review_prompt(case);
        if let Some(leak) = case_leak(&p, case) {
            panic!("review prompt leaks the answer key ({leak})");
        }
        return p;
    }
    let n = std::cmp::max(1, approx_tokens / 12);
    let mut body = String::with_capacity((n as usize) * 52);
    for i in 0..n {
        body.push_str(&format!(
            "record {}: telemetry sample, no anomalies detected; ",
            seed * 100_000 + i as u64
        ));
    }
    format!("Run {seed}. Context follows.\n{body}\n\n{}", scen.prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case() -> Case {
        Case {
            id: "laravel-balance-race".into(),
            language: "PHP / Laravel".into(),
            defect: "concurrency".into(),
            expected: "Read-modify-write on the balance outside any transaction".into(),
            code: "<?php class WalletService {}".into(),
            signals: vec![],
        }
    }

    #[test]
    fn prompts_are_unique_per_seed() {
        let s = scenario("chat").unwrap();
        assert_ne!(build_prompt(512, 1, s, &[]), build_prompt(512, 2, s, &[]));
        assert_eq!(build_prompt(512, 1, s, &[]), build_prompt(512, 1, s, &[]));
    }

    #[test]
    fn review_prompt_carries_no_answer_key() {
        let c = case();
        let p = review_prompt(&c);
        assert!(!p.contains(&c.id));
        assert!(!p.to_lowercase().contains("concurrency"));
        assert!(case_leak(&p, &c).is_none());
    }

    #[test]
    fn leak_detector_catches_a_leaky_prompt() {
        let c = case();
        assert!(case_leak("find the concurrency bug", &c).is_some());
        assert!(case_leak(&c.expected, &c).is_some());
        assert!(case_leak("review this file", &c).is_none());
        // "laravel" is legitimately in the language line, so it must not trip the id check.
        assert!(case_leak("Review this PHP / Laravel file", &c).is_none());
    }

    #[test]
    fn scenario_instruction_reaches_the_prompt() {
        let s = scenario("code").unwrap();
        assert!(build_prompt(128, 1, s, &[]).contains("merge_ranges"));
    }
}
