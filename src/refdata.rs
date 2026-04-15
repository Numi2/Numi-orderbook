use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstrumentTick {
    pub instr: u32,
    pub tick: i64,
}

pub fn load_instrument_ticks(path: &Path) -> Result<Vec<InstrumentTick>> {
    let file = File::open(path).with_context(|| format!("open tick table {:?}", path))?;
    parse_instrument_ticks(BufReader::new(file))
        .with_context(|| format!("parse tick table {:?}", path))
}

pub fn parse_instrument_ticks(reader: impl BufRead) -> Result<Vec<InstrumentTick>> {
    let mut rows = Vec::new();
    let mut header = None;

    for (line_no, line) in reader.lines().enumerate() {
        let line_no = line_no + 1;
        let line = line.with_context(|| format!("read line {line_no}"))?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() < 2 {
            anyhow::bail!("line {line_no}: expected at least instr,tick columns");
        }

        if header.is_none() && looks_like_header(&fields) {
            header = Some(resolve_header(&fields, line_no)?);
            continue;
        }

        let (instr_idx, tick_idx) = header.unwrap_or((0, 1));
        let Some(instr_s) = fields.get(instr_idx) else {
            anyhow::bail!("line {line_no}: missing instrument column {instr_idx}");
        };
        let Some(tick_s) = fields.get(tick_idx) else {
            anyhow::bail!("line {line_no}: missing tick column {tick_idx}");
        };

        let instr = instr_s
            .parse::<u32>()
            .with_context(|| format!("line {line_no}: invalid instrument id {instr_s:?}"))?;
        let tick = tick_s
            .parse::<i64>()
            .with_context(|| format!("line {line_no}: invalid tick {tick_s:?}"))?;
        if tick <= 0 {
            anyhow::bail!("line {line_no}: tick must be > 0");
        }
        rows.push(InstrumentTick { instr, tick });
    }

    Ok(rows)
}

fn looks_like_header(fields: &[&str]) -> bool {
    fields
        .iter()
        .any(|field| field.chars().any(|c| c.is_ascii_alphabetic()))
}

fn resolve_header(fields: &[&str], line_no: usize) -> Result<(usize, usize)> {
    let mut instr_idx = None;
    let mut tick_idx = None;
    for (idx, field) in fields.iter().enumerate() {
        let normalized = field.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "instr" | "instrument" | "instrument_id" | "security_id" | "secid" => {
                instr_idx = Some(idx)
            }
            "tick"
            | "tick_size"
            | "price_tick"
            | "minimum_price_increment"
            | "min_price_increment" => tick_idx = Some(idx),
            _ => {}
        }
    }

    let Some(instr_idx) = instr_idx else {
        anyhow::bail!("line {line_no}: header missing instrument id column");
    };
    let Some(tick_idx) = tick_idx else {
        anyhow::bail!("line {line_no}: header missing tick column");
    };
    Ok((instr_idx, tick_idx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_headered_tick_table() {
        let input = Cursor::new(
            b"# venue reference data\nsecurity_id,symbol,minimum_price_increment\n1001,ABC,5\n1002,XYZ,10\n",
        );
        let rows = parse_instrument_ticks(input).unwrap();
        assert_eq!(
            rows,
            vec![
                InstrumentTick {
                    instr: 1001,
                    tick: 5
                },
                InstrumentTick {
                    instr: 1002,
                    tick: 10
                },
            ]
        );
    }

    #[test]
    fn parses_two_column_tick_table_without_header() {
        let input = Cursor::new(b"1001,5\n1002,10\n");
        let rows = parse_instrument_ticks(input).unwrap();
        assert_eq!(
            rows[0],
            InstrumentTick {
                instr: 1001,
                tick: 5
            }
        );
        assert_eq!(
            rows[1],
            InstrumentTick {
                instr: 1002,
                tick: 10
            }
        );
    }

    #[test]
    fn rejects_non_positive_tick() {
        let input = Cursor::new(b"instr,tick\n1001,0\n");
        assert!(parse_instrument_ticks(input).is_err());
    }
}
