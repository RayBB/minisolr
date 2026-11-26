//! Command line entry point: `serve` the index over HTTP or check analyzer `parity`.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::ExitCode;

use minisolr_rs::Error;
use minisolr_rs::analysis::Analyzer;
use minisolr_rs::schema::{Kind, Schema};
use minisolr_rs::server::serve;
use minisolr_rs::store::Store;
use serde_json::Value;

const USAGE: &str = "usage:\n  minisolr-rs serve <conf-dir> <docs.jsonl> [addr=127.0.0.1:8983]\n  minisolr-rs parity <conf-dir> <tokens.jsonl>\n\n<conf-dir> holds managed-schema.xml and enumsConfig.xml.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["serve", conf, docs] => run_serve(conf, docs, "127.0.0.1:8983"),
        ["serve", conf, docs, addr] => run_serve(conf, docs, addr),
        ["parity", conf, tokens] => parity(conf, Path::new(tokens)),
        _ => Err(Error::Other(USAGE.into())),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn run_serve(conf: &str, docs: &str, addr: &str) -> Result<(), Error> {
    let schema = Schema::load(Path::new(conf))?;
    serve(&Store::load(schema, Path::new(docs))?, addr)
}

/// Compares Rust index-time tokens with Python's, dumped as JSON lines of
/// `{"field", "value", "main", "extra"}`; prints per-field match rates and mismatches.
/// Each field's chain comes from the schema; rows for non-text fields are skipped.
fn parity(conf: &str, path: &Path) -> Result<(), Error> {
    let schema = Schema::load(Path::new(conf))?;
    let analyzer = Analyzer::new();
    let mut stats: Vec<(String, usize, usize)> = Vec::new();
    let mut shown = 0;
    for line in BufReader::new(std::fs::File::open(path)?).lines() {
        let row: Value = serde_json::from_str(&line?)?;
        let name = row.get("field").and_then(Value::as_str).unwrap_or_default();
        let (Some(field), Some(value)) =
            (schema.get(name), row.get("value").and_then(Value::as_str))
        else {
            continue;
        };
        if field.kind != Kind::Text {
            continue;
        }
        let chain = field.chain;
        let strings = |key: &str| -> Vec<String> {
            row.get(key)
                .and_then(Value::as_array)
                .map_or_else(Vec::new, |a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
        };
        let want = (strings("main"), strings("extra"));
        let got = analyzer.index(chain, value);
        let ok = (got.main.clone(), got.extra.clone()) == want;
        match stats.iter_mut().find(|(n, ..)| n == name) {
            Some(entry) => {
                entry.1 += 1;
                entry.2 += usize::from(ok);
            }
            None => stats.push((name.to_owned(), 1, usize::from(ok))),
        }
        if !ok && shown < 40 {
            shown += 1;
            println!(
                "MISMATCH {name} {value:?}\n  python: {:?} + {:?}\n  rust:   {:?} + {:?}",
                want.0, want.1, got.main, got.extra
            );
        }
    }
    let (total, matched) = stats
        .iter()
        .fold((0, 0), |(t, m), (_, n, ok)| (t + n, m + ok));
    for (name, n, ok) in &stats {
        println!("{name}: {ok}/{n} exact ({:.1}%)", percent(*ok, *n));
    }
    println!("all: {matched}/{total}");
    Ok(())
}

fn percent(part: usize, whole: usize) -> f64 {
    let to_f64 = |n: usize| f64::from(u32::try_from(n).unwrap_or(u32::MAX));
    100.0 * to_f64(part) / to_f64(whole)
}
