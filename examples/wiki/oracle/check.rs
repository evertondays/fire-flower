//! Contagem de divergências entre o motor e o oráculo, por tipo de consulta.

use serde_json::{Value, json};

#[derive(Default)]
pub(crate) struct Check {
    pub(crate) name: &'static str,
    pub(crate) checked: usize,
    pub(crate) divergences: usize,
    pub(crate) examples: Vec<String>,
    pub(crate) notes: Vec<String>,
}

impl Check {
    pub(crate) fn new(name: &'static str) -> Self {
        Self { name, ..Default::default() }
    }

    pub(crate) fn compare<T: PartialEq + std::fmt::Debug>(&mut self, what: &str, got: &T, expected: &T) {
        self.verdict(what, got == expected, || (format!("{got:?}"), format!("{expected:?}")));
    }

    /// Registra um caso; `show` só roda se divergiu (e só para os 5 primeiros).
    pub(crate) fn verdict(&mut self, what: &str, ok: bool, show: impl FnOnce() -> (String, String)) {
        self.checked += 1;
        if !ok {
            self.divergences += 1;
            if self.examples.len() < 5 {
                let cut = |s: String| {
                    if s.len() > 200 { format!("{}…", &s[..s.char_indices().nth(200).map_or(s.len(), |(i, _)| i)]) } else { s }
                };
                let (got, expected) = show();
                self.examples.push(format!("{what}: motor={} oráculo={}", cut(got), cut(expected)));
            }
        }
    }

    pub(crate) fn json(&self) -> Value {
        json!({ "check": self.name, "cases": self.checked, "divergences": self.divergences,
                "examples": self.examples, "notes": self.notes })
    }
}
