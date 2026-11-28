//! Command line entry point: `serve` the index over HTTP or check analyzer `parity`.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use minisolr::Error;
use minisolr::schema::{FieldType, Kind, Schema};
use minisolr::server::{Core, serve};
use minisolr::store::Store;
use serde_json::Value;

const USAGE: &str = "usage:\n  minisolr serve [--strict] <conf-dir> <data-dir | docs.jsonl> [addr=127.0.0.1:8983]\n  minisolr parity <conf-dir> <tokens.jsonl>\n  minisolr ping [addr=127.0.0.1:8983]\n\n<conf-dir> holds managed-schema.xml and enumsConfig.xml. A data directory (created if\nmissing) persists updates in <data-dir> (docs.jsonl snapshot plus docs.wal log); a .jsonl file is loaded and updates\nstay in memory. --strict (or MINISOLR_STRICT=1) answers HTTP 400 for\nrequests that use unsupported features instead of an empty or partial result. `ping` exits 0 if the server at addr answers its ping (a healthcheck).";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut strict =
        std::env::var("MINISOLR_STRICT").is_ok_and(|v| !matches!(v.as_str(), "" | "0"));
    if args.first().is_some_and(|a| a == "serve") && args.iter().any(|a| a == "--strict") {
        args.retain(|a| a != "--strict");
        strict = true;
    }
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["serve", conf, docs] => run_serve(conf, docs, "127.0.0.1:8983", strict),
        ["serve", conf, docs, addr] => run_serve(conf, docs, addr, strict),
        ["parity", conf, tokens] => parity(conf, Path::new(tokens)),
        ["ping"] => minisolr::ping::ping("127.0.0.1:8983"),
        ["ping", addr] => minisolr::ping::ping(addr),
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

fn run_serve(conf: &str, data: &str, addr: &str, strict: bool) -> Result<(), Error> {
    let schema = Arc::new(Schema::load(Path::new(conf))?);
    let warnings = schema.analysis_warnings();
    if !warnings.is_empty() {
        eprintln!(
            "warning: analyzer components with no implementation are skipped (filters) or use the standard tokenizer, as in Python, in {} field types: {}",
            warnings.len(),
            warnings.join("; ")
        );
    }
    let data = Path::new(data);
    let store = if data.is_file() {
        let mut store = Store::new(Arc::clone(&schema), None);
        store.load_file(data)?;
        store
    } else {
        std::fs::create_dir_all(data)?;
        Store::open(Arc::clone(&schema), data)?
    };
    serve(
        &Arc::new(Core::new(store, schema).with_strict(strict)),
        addr,
    )
}

/// Compares Rust tokens with Python's, dumped as JSON lines of `{"field" or "type",
/// "value", "main", "extra"}` plus an optional `"query"` (the positions with their
/// alternatives, compared too); prints per-field (or per-type) match rates and the first
/// mismatches. `field` rows take the chain from the field's type and skip non-text fields.
fn parity(conf: &str, path: &Path) -> Result<(), Error> {
    let schema = Schema::load(Path::new(conf))?;
    let mut stats: Vec<(String, usize, usize)> = Vec::new();
    let mut shown = 0;
    for line in BufReader::new(std::fs::File::open(path)?).lines() {
        let row: Value = serde_json::from_str(&line?)?;
        let text = |key: &str| row.get(key).and_then(Value::as_str);
        let Some(value) = text("value") else {
            continue;
        };
        let (name, ftype) = if let Some(t) = text("type") {
            (t, schema.types.get(t))
        } else {
            let name = text("field").unwrap_or_default();
            let ftype = schema
                .get(name)
                .filter(|f| f.kind == Kind::Text)
                .and_then(|f| schema.field_type(&f).cloned());
            let Some(ftype) = ftype else {
                continue;
            };
            check(&mut stats, &mut shown, name, value, &row, &ftype);
            continue;
        };
        if let Some(ftype) = ftype {
            check(&mut stats, &mut shown, name, value, &row, ftype);
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

fn check(
    stats: &mut Vec<(String, usize, usize)>,
    shown: &mut usize,
    name: &str,
    value: &str,
    row: &Value,
    ftype: &FieldType,
) {
    let strings = |v: &Value| -> Vec<String> {
        v.as_array().map_or_else(Vec::new, |a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
    };
    let field = |key: &str| strings(row.get(key).unwrap_or(&Value::Null));
    let want = (field("main"), field("extra"));
    let got = ftype.index.index(value);
    let mut ok = (got.main.clone(), got.extra.clone()) == want;
    let want_query: Option<Vec<Vec<String>>> = row
        .get("query")
        .and_then(Value::as_array)
        .map(|positions| positions.iter().map(strings).collect());
    let got_query = ftype.query.query(value);
    if let Some(q) = &want_query {
        ok &= *q == got_query;
    }
    match stats.iter_mut().find(|(n, ..)| n == name) {
        Some(entry) => {
            entry.1 += 1;
            entry.2 += usize::from(ok);
        }
        None => stats.push((name.to_owned(), 1, usize::from(ok))),
    }
    if !ok && *shown < 60 {
        *shown += 1;
        println!(
            "MISMATCH {name} {value:?}\n  python: {:?} + {:?} query {:?}\n  rust:   {:?} + {:?} query {:?}",
            want.0, want.1, want_query, got.main, got.extra, got_query
        );
    }
}

fn percent(part: usize, whole: usize) -> f64 {
    let to_f64 = |n: usize| f64::from(u32::try_from(n).unwrap_or(u32::MAX));
    100.0 * to_f64(part) / to_f64(whole)
}
