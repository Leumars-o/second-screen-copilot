//! Markdown report builder.

use crate::stats::Summary;
use std::fmt::Write as _;
use std::path::Path;

#[derive(Debug, Default, Clone)]
pub struct Report {
    out: String,
}

impl Report {
    pub fn new(title: &str) -> Self {
        let mut r = Self::default();
        let _ = writeln!(r.out, "# {title}\n");
        r
    }

    pub fn h2(&mut self, s: &str) -> &mut Self {
        let _ = writeln!(self.out, "## {s}\n");
        self
    }

    pub fn para(&mut self, s: &str) -> &mut Self {
        let _ = writeln!(self.out, "{s}\n");
        self
    }

    pub fn bullet(&mut self, s: &str) -> &mut Self {
        let _ = writeln!(self.out, "- {s}");
        self
    }

    /// End a bullet list (blank line).
    pub fn end_list(&mut self) -> &mut Self {
        self.out.push('\n');
        self
    }

    pub fn table<S: AsRef<str>>(&mut self, header: &[&str], rows: &[Vec<S>]) -> &mut Self {
        let esc = |s: &str| s.replace('|', "\\|").replace('\n', " ");
        let _ = writeln!(self.out, "| {} |", header.join(" | "));
        let _ = writeln!(self.out, "|{}", " --- |".repeat(header.len()));
        for r in rows {
            let cells: Vec<String> = r.iter().map(|c| esc(c.as_ref())).collect();
            let _ = writeln!(self.out, "| {} |", cells.join(" | "));
        }
        self.out.push('\n');
        self
    }

    pub fn as_str(&self) -> &str {
        &self.out
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, &self.out)
    }
}

/// `p50 / p95 (n)` in ms, or `n/a`.
pub fn ms_cell(s: Option<Summary>) -> String {
    match s {
        Some(s) => format!("{} / {} (n={})", s.p50, s.p95, s.n),
        None => "n/a".into(),
    }
}

pub fn pct(num: usize, den: usize) -> String {
    if den == 0 { "n/a".into() } else { format!("{:.1}% ({num}/{den})", 100.0 * num as f64 / den as f64) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_escapes_pipes() {
        let mut r = Report::new("T");
        r.table(&["a", "b"], &[vec!["x|y", "z"]]);
        assert!(r.as_str().contains("| x\\|y | z |"));
        assert_eq!(pct(1, 4), "25.0% (1/4)");
        assert_eq!(pct(0, 0), "n/a");
    }
}
