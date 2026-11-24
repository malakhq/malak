//! Aligned tables and their narrow-terminal alternatives.

use super::{Format, display, width};
use std::fmt::Write as _;

impl Format {
    pub(in crate::cli) fn table(&self, headers: &[&str], rows: &[Vec<String>]) -> String {
        let rows = display_rows(rows);
        let widths = table_widths(headers, &rows);
        if widths.iter().sum::<usize>() + headers.len().saturating_sub(1) * 2 + 2 > self.width {
            let mut out = String::new();
            for (index, row) in rows.iter().enumerate() {
                if index != 0 {
                    out.push('\n');
                }
                for (&header, value) in headers.iter().zip(row) {
                    let prefix = format!("  {header}  ");
                    self.wrapped(&mut out, &prefix, value, true);
                }
            }
            return out;
        }
        self.aligned_table(headers, &rows, &widths, &[])
    }

    pub(in crate::cli) fn name_table(
        &self,
        headers: &[&str],
        rows: &[Vec<String>],
        preserve_name: impl Fn(&str) -> bool,
    ) -> String {
        if !self.interactive {
            return self.table(headers, rows);
        }
        let mut rows = display_rows(rows);
        let mut widths = table_widths(headers, &rows);
        let name_width = rows
            .iter()
            .map(|row| row[0].as_str())
            .filter(|name| preserve_name(name))
            .map(width)
            .max()
            .unwrap_or_default()
            .max(width(headers[0]))
            .max(widths[0].min(12));
        let mut columns = widths.len();
        while columns > 1
            && name_width + widths[1..columns].iter().sum::<usize>() + columns * 2 > self.width
        {
            columns -= 1;
        }
        let available = self
            .width
            .saturating_sub(widths[1..columns].iter().sum::<usize>() + columns * 2);
        widths[0] = widths[0].min(available).max(name_width);
        for row in &mut rows {
            row[0] = shorten_name(&row[0], widths[0]);
        }
        self.aligned_table(
            &headers[..columns],
            &rows,
            &widths[..columns],
            &headers[columns..],
        )
    }

    fn aligned_table(
        &self,
        headers: &[&str],
        rows: &[Vec<String>],
        widths: &[usize],
        continuations: &[&str],
    ) -> String {
        let mut out = String::new();
        let numeric = (0..headers.len())
            .map(|index| {
                let cells = || rows.iter().map(|row| row[index].as_str());
                index > 0
                    && cells().any(|cell| cell.parse::<u64>().is_ok())
                    && cells().all(|cell| cell.parse::<u64>().is_ok() || cell == "Unavailable")
            })
            .collect::<Vec<_>>();
        let rule = |out: &mut String, row: &[&str]| {
            out.push_str("  ");
            for (index, (cell, &space)) in row.iter().zip(widths).enumerate() {
                let padding = space.saturating_sub(width(cell));
                if numeric[index] {
                    write!(out, "{:padding$}{cell}", "").unwrap();
                } else {
                    out.push_str(cell);
                    if index + 1 < row.len() {
                        write!(out, "{:padding$}", "").unwrap();
                    }
                }
                if index + 1 < row.len() {
                    out.push_str("  ");
                }
            }
            out.push('\n');
        };
        rule(&mut out, headers);
        for row in rows {
            rule(
                &mut out,
                &row[..headers.len()]
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            );
            if !continuations.is_empty() {
                let fields = continuations
                    .iter()
                    .copied()
                    .zip(row[headers.len()..].iter().map(String::as_str))
                    .collect::<Vec<_>>();
                self.render_fields(&mut out, &fields, true, "    ");
            }
        }
        out
    }
}

fn display_rows(rows: &[Vec<String>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| row.iter().map(|cell| display(cell)).collect())
        .collect()
}

fn table_widths(headers: &[&str], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<_> = headers.iter().map(|heading| width(heading)).collect();
    for row in rows {
        for (column, cell) in widths.iter_mut().zip(row) {
            *column = (*column).max(width(cell));
        }
    }
    widths
}

fn shorten_name(name: &str, columns: usize) -> String {
    if width(name) <= columns {
        return name.to_owned();
    }
    let end = name
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|&index| width(&name[..index]) < columns)
        .last()
        .unwrap_or_default();
    format!("{}…", &name[..end])
}
