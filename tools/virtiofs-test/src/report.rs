// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pass/fail tally, printed one line per check.
//!
//! Every line starts with `VFSTEST:` so the host can grep the console
//! log without parsing around kernel printk output.

use std::io::Write;

pub const PREFIX: &str = "VFSTEST:";

/// Outcome of one check.
pub enum Outcome {
    Pass(String),
    Fail(String),
}

pub type Check = Result<String, String>;

pub struct Report {
    pass: u32,
    fail: u32,
}

impl Report {
    pub fn new() -> Self {
        Self { pass: 0, fail: 0 }
    }

    pub fn record(&mut self, name: &str, outcome: Outcome) {
        let (tag, detail) = match outcome {
            Outcome::Pass(d) => {
                self.pass += 1;
                ("PASS", d)
            }
            Outcome::Fail(d) => {
                self.fail += 1;
                ("FAIL", d)
            }
        };
        emit(&format!("{PREFIX} {tag} {name} {detail}"));
    }

    /// Run one check and record what it returned.
    pub fn run(&mut self, name: &str, f: impl FnOnce() -> Check) {
        let outcome = match f() {
            Ok(d) => Outcome::Pass(d),
            Err(d) => Outcome::Fail(d),
        };
        self.record(name, outcome);
    }

    pub fn note(&self, msg: &str) {
        emit(&format!("{PREFIX} NOTE {msg}"));
    }

    /// Print the totals. Returns true when nothing failed.
    pub fn finish(&self) -> bool {
        let ok = self.fail == 0;
        emit(&format!(
            "{PREFIX} DONE pass={} fail={} result={}",
            self.pass,
            self.fail,
            if ok { "OK" } else { "FAILED" }
        ));
        ok
    }
}

/// Write one line to the console and flush it.
///
/// The flush matters: this process powers the VM off as soon as it
/// returns, and a buffered line would never reach the host.
fn emit(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}
