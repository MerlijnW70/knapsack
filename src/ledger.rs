//! Session memory + residency state — what Rucksack lacks. Tracks which block handles
//! have been shown, whether each is still RESIDENT in the model's context window, and an
//! estimate of its token weight (for budget-based eviction).
//!
//! Cardinal rule: only emit a "unchanged, already in context" back-reference when
//! residency is `Resident`. A backref to something paged out is a dangling pointer.
//! Until eviction can be driven from the live transcript, we approximate residency with
//! a conservative token budget: when the resident set exceeds it, evict OLDEST first.
//! Persisted as TSV (`handle<TAB>code<TAB>step<TAB>tokens`); zero-dep.

use crate::hash::Handle;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Residency {
    Resident,
    Evicted,
    Unknown,
}

pub struct Ledger {
    path: Option<PathBuf>,
    map: HashMap<String, (u8, u64, usize)>, // handle -> (code, step, tokens); 0=Resident 1=Evicted
}

fn code_to_res(c: u8) -> Residency {
    match c {
        0 => Residency::Resident,
        1 => Residency::Evicted,
        _ => Residency::Unknown,
    }
}

impl Ledger {
    pub fn in_memory() -> Self {
        Ledger {
            path: None,
            map: HashMap::new(),
        }
    }

    pub fn load(path: PathBuf) -> Self {
        let mut map = HashMap::new();
        if let Ok(text) = fs::read_to_string(&path) {
            for line in text.lines() {
                let f: Vec<&str> = line.split('\t').collect();
                if f.len() >= 4 {
                    if let (Ok(c), Ok(s), Ok(t)) = (
                        f[1].parse::<u8>(),
                        f[2].parse::<u64>(),
                        f[3].parse::<usize>(),
                    ) {
                        map.insert(f[0].to_string(), (c, s, t));
                    }
                }
            }
        }
        Ledger {
            path: Some(path),
            map,
        }
    }

    pub fn save(&self) {
        if let Some(p) = &self.path {
            if let Some(parent) = p.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let mut s = String::new();
            for (h, (c, step, tok)) in &self.map {
                s.push_str(&format!("{}\t{}\t{}\t{}\n", h, c, step, tok));
            }
            let _ = fs::write(p, s);
        }
    }

    pub fn residency(&self, h: &Handle) -> Residency {
        self.map
            .get(h)
            .map(|&(c, _, _)| code_to_res(c))
            .unwrap_or(Residency::Unknown)
    }

    pub fn note(&mut self, h: &Handle, step: u64, tokens: usize) {
        self.map.insert(h.clone(), (0, step, tokens));
    }

    pub fn evict(&mut self, h: &Handle) {
        if let Some(v) = self.map.get_mut(h) {
            v.0 = 1;
        }
    }

    /// Estimated tokens currently considered resident.
    pub fn resident_tokens(&self) -> usize {
        self.map.values().filter(|v| v.0 == 0).map(|v| v.2).sum()
    }

    /// Evict oldest resident handles until the resident set fits the budget. Returns the
    /// number evicted. This is what keeps delta back-references honest without pretending
    /// to know the platform's exact context-window state.
    pub fn enforce_budget(&mut self, budget: usize) -> usize {
        if self.resident_tokens() <= budget {
            return 0;
        }
        let mut resident: Vec<(String, u64, usize)> = self
            .map
            .iter()
            .filter(|(_, v)| v.0 == 0)
            .map(|(k, v)| (k.clone(), v.1, v.2))
            .collect();
        resident.sort_by_key(|x| x.1); // oldest step first
        let total: usize = resident.iter().map(|x| x.2).sum();
        let mut over = total.saturating_sub(budget);
        let mut n = 0;
        for (k, _, tk) in resident {
            if over == 0 {
                break;
            }
            if let Some(v) = self.map.get_mut(&k) {
                v.0 = 1;
            }
            over = over.saturating_sub(tk);
            n += 1;
        }
        n
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}
