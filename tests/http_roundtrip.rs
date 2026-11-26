//! End-to-end checks against the real HTTP server on a tiny data set.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::thread;
use std::time::Duration;

use minisolr_rs::analysis::{Analyzer, Chain};
use minisolr_rs::schema::Schema;
use minisolr_rs::server::{Core, serve};
use minisolr_rs::store::Store;
use serde_json::Value;
use std::sync::Arc;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn free_addr() -> TestResult<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(format!("127.0.0.1:{}", listener.local_addr()?.port()))
}

fn request(addr: &str, head: &str, body: &str) -> TestResult<Value> {
    let mut stream = TcpStream::connect(addr)?;
    let msg = format!(
        "{head}\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(msg.as_bytes())?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw)?;
    let (_, json) = raw.split_once("\r\n\r\n").ok_or("no http body")?;
    Ok(serde_json::from_str(json)?)
}

/// Starts a server on a free port: a `.jsonl` file is loaded in memory, a directory is
/// opened as a persistent data directory.
fn start(data: &Path) -> TestResult<String> {
    let schema = Arc::new(Schema::load(Path::new("tests/data/conf"))?);
    let store = if data.is_file() {
        let mut store = Store::new(Arc::clone(&schema), None);
        store.load_file(data)?;
        store
    } else {
        Store::open(Arc::clone(&schema), data)?
    };
    let core = Arc::new(Core::new(store, schema));
    let addr = free_addr()?;
    let bound = addr.clone();
    thread::spawn(move || serve(&core, &bound));
    thread::sleep(Duration::from_millis(300));
    Ok(addr)
}

#[test]
fn serves_select_ping_and_post_forms() -> TestResult {
    let addr = start(Path::new("tests/data/mini.jsonl"))?;

    let ping = request(&addr, "GET /solr/openlibrary/admin/ping HTTP/1.1", "")?;
    assert_eq!(ping["status"], "OK");

    let q = "q=%7B!edismax+q.op%3DAND+qf%3D%22alternative_title+author_name%22+v%3D%24uq%7D&uq=alice+carroll&fq=type%3Awork&fl=key&rows=5";
    let got = request(
        &addr,
        &format!("GET /solr/openlibrary/select?{q} HTTP/1.1"),
        "",
    )?;
    assert_eq!(got["response"]["numFound"], 1);
    assert_eq!(got["response"]["docs"][0]["key"], "/works/OL2W");

    let posted = request(
        &addr,
        "POST /solr/openlibrary/select HTTP/1.1\r\nContent-Type: application/x-www-form-urlencoded",
        "q=author_name%3Acarroll&fq=type%3Awork&sort=edition_count+desc&fl=key",
    )?;
    assert_eq!(posted["response"]["numFound"], 2);
    assert_eq!(posted["response"]["docs"][0]["key"], "/works/OL2W");

    let accents = request(
        &addr,
        "GET /solr/openlibrary/select?q=garcia+marquez&defType=edismax&qf=author_name&q.op=AND&fl=key HTTP/1.1",
        "",
    )?;
    assert_eq!(accents["response"]["numFound"], 1);

    let odd = request(&addr, "GET /solr/openlibrary/select?q=ti*e HTTP/1.1", "")?;
    assert_eq!(odd["response"]["numFound"], 0);
    assert!(odd["responseHeader"]["warning"].is_string());
    Ok(())
}

fn form(pairs: &[(&str, &str)]) -> String {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    char::from(b).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn keys(v: &Value) -> Vec<&str> {
    v.pointer("/response/docs")
        .and_then(Value::as_array)
        .map(|docs| {
            docs.iter()
                .filter_map(|d| d.get("key").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn book_search_shape_with_block_join_ranges_and_or() -> TestResult {
    let addr = start(Path::new("tests/data/mini.jsonl"))?;
    let post = |pairs: &[(&str, &str)]| {
        request(
            &addr,
            "POST /solr/openlibrary/select HTTP/1.1\r\nContent-Type: application/x-www-form-urlencoded",
            &form(pairs),
        )
    };

    let book_search = [
        (
            "q",
            "+({!edismax q.op=\"AND\" qf=\"alternative_title^40 author_name^40\" pf=\"alternative_title^50\" boost=\"sum(1,edition_count)\" v=$userWorkQuery}) +(_query_:\"{!parent which=type:work v=$fullEdFilterQuery filters=$editions.fq}\" OR edition_count:0)",
        ),
        ("userWorkQuery", "carroll"),
        (
            "fullEdFilterQuery",
            "({!edismax v=$userEdFilterQuery qf=\"title publisher\"})",
        ),
        ("userEdFilterQuery", "+language:eng"),
        ("editions.fq", "type:edition"),
        ("editions.fq", "ebook_access:[borrowable TO *]"),
        ("fq", "type:work"),
        ("fl", "key,editions:[subquery]"),
    ];
    let got = post(&book_search)?;
    assert!(got["responseHeader"]["warning"].is_null(), "{got}");
    assert_eq!(got["response"]["numFound"], 1);
    assert_eq!(keys(&got), ["/works/OL2W"]);

    let ranges = post(&[
        ("q", "*:*"),
        (
            "fq",
            "edition_count:[20 TO *] OR first_publish_year:[* TO 1000}",
        ),
        ("sort", "edition_count desc"),
        ("fl", "key"),
    ])?;
    assert_eq!(keys(&ranges), ["/works/OL2W", "/works/OL1W"]);

    let terms = post(&[
        ("q", "{!terms f=key v=/works/OL1W,/works/OL4W}"),
        ("fl", "key"),
    ])?;
    assert_eq!(terms["response"]["numFound"], 2);

    let unsupported = post(&[("q", "title:[a TO b]")])?;
    assert_eq!(unsupported["response"]["numFound"], 0);
    assert!(unsupported["responseHeader"]["warning"].is_string());
    Ok(())
}

#[test]
fn analyzers_match_recorded_python_output() {
    let a = Analyzer::new();
    let en = |t| a.index(Chain::EnSplitting, t).main;
    assert_eq!(en("The Odyssey"), ["the", "odyssei"]);
    assert_eq!(
        en("Three cups of tea: one man's mission"),
        ["three", "cup", "of", "tea", "on", "man", "mission"]
    );
    assert_eq!(
        a.index(Chain::EnSplitting, "promote peace--one").extra,
        ["peaceon"]
    );
}

#[test]
fn function_boosts_and_sorts() -> TestResult {
    let addr = start(Path::new("tests/data/mini.jsonl"))?;
    let post = |pairs: &[(&str, &str)]| {
        request(
            &addr,
            "POST /solr/openlibrary/select HTTP/1.1\r\nContent-Type: application/x-www-form-urlencoded",
            &form(pairs),
        )
    };
    let works = [("q", "*:*"), ("fq", "type:work"), ("fl", "key")];
    let with = |extra: &[(&str, &str)]| {
        let mut p = works.to_vec();
        p.extend_from_slice(extra);
        post(&p)
    };

    let boosted = with(&[
        ("defType", "edismax"),
        ("qf", "title"),
        ("boost", "log(edition_count)"),
        ("rows", "2"),
    ])?;
    assert_eq!(boosted["response"]["numFound"], 4);
    assert_eq!(keys(&boosted), ["/works/OL2W", "/works/OL1W"]);

    let bf = with(&[
        ("defType", "edismax"),
        ("qf", "title"),
        ("bf", "sub(0,edition_count)"),
        ("rows", "1"),
    ])?;
    assert_eq!(
        keys(&bf),
        ["/works/OL4W"],
        "every hit is scored before the page"
    );

    let by_year = with(&[("sort", "def(first_publish_year,9999) desc, key asc")])?;
    assert_eq!(
        keys(&by_year),
        ["/works/OL4W", "/works/OL3W", "/works/OL2W", "/works/OL1W"]
    );

    let random = with(&[("sort", "random_1 asc")])?;
    // Python: sorted by int(md5("random_1" + key).hexdigest()[:8], 16).
    assert_eq!(
        keys(&random),
        ["/works/OL1W", "/works/OL4W", "/works/OL3W", "/works/OL2W"]
    );

    let enums = post(&[
        ("q", "type:edition"),
        ("sort", "ebook_access desc"),
        ("fl", "key"),
    ])?;
    assert_eq!(keys(&enums), ["/books/OL3M", "/books/OL1M", "/books/OL2M"]);
    Ok(())
}

#[test]
// One ordered scenario on one server; its steps depend on each other.
#[allow(clippy::too_many_lines)]
fn update_lifecycle_and_restart_from_the_data_directory() -> TestResult {
    let dir = std::env::temp_dir().join(format!("minisolr-rs-http-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let addr = start(&dir)?;
    let update = |body: &str, query: &str| {
        request(
            &addr,
            &format!(
                "POST /solr/openlibrary/update{query} HTTP/1.1\r\nContent-Type: application/json"
            ),
            body,
        )
    };
    let select = |pairs: &[(&str, &str)]| {
        request(
            &addr,
            "POST /solr/openlibrary/select HTTP/1.1\r\nContent-Type: application/x-www-form-urlencoded",
            &form(pairs),
        )
    };
    let get = |q: &str| {
        request(
            &addr,
            &format!("GET /solr/openlibrary/get?{q} HTTP/1.1"),
            "",
        )
    };

    // Open Library's shape: repeated `add` commands, nested editions, then a commit.
    let body = r#"{"add": {"doc": {"key": "/works/OL1W", "type": "work", "title": "Alice in Wonderland",
        "author_name": ["Lewis Carroll"], "edition_count": 2,
        "editions": [{"key": "/books/OL1M", "type": "edition", "title": "Alice", "language": ["eng"]},
                     {"key": "/books/OL2M", "type": "edition", "title": "Alice {braces} \"quoted\"", "language": ["fre"]}]}},
      "add": {"doc": {"key": "/works/OL2W", "type": "work", "title": "Tom Sawyer", "author_name": ["Mark Twain"], "edition_count": 1}},
      "add": {"doc": {"key": "/authors/OL1A", "type": "author", "name": "Lewis Carroll"}},
      "commit": {}}"#;
    let added = update(body, "?update.partial.requireInPlace=true")?;
    assert_eq!(added["responseHeader"]["status"], 0, "{added}");
    assert_eq!(select(&[("q", "*:*")])?["response"]["numFound"], 5);
    let works = select(&[
        ("q", "author_name:carroll"),
        ("fq", "type:work"),
        ("fl", "key,score,t:title,editions:[subquery]"),
        ("editions.q", "{!terms f=_root_ v=$row.key}"),
        ("editions.fq", "language:fre"),
        ("editions.fl", "key"),
    ])?;
    assert_eq!(keys(&works), ["/works/OL1W"]);
    let doc = &works["response"]["docs"][0];
    assert_eq!(doc["t"], "Alice in Wonderland");
    assert!(doc["score"].as_f64().is_some_and(|s| s > 0.0));
    assert_eq!(doc["editions"]["numFound"], 1);
    assert_eq!(
        doc["editions"]["docs"],
        serde_json::json!([{"key": "/books/OL2M"}])
    );
    let child = select(&[("q", "key:\"/works/OL1W\""), ("fl", "key,[child]")])?;
    let kids = &child["response"]["docs"][0]["_childDocuments_"];
    assert_eq!(kids.as_array().map(Vec::len), Some(2));
    assert_eq!(kids[1]["_nest_path_"], "/editions#1");

    let atomic = update(
        r#"[{"key": "/works/OL1W", "edition_count": {"inc": 4}, "title": {"set": "Alice II"}, "subject": {"add": "Fantasy"}}]"#,
        "?commit=true",
    )?;
    assert_eq!(atomic["responseHeader"]["status"], 0);
    let got = get("id=/works/OL1W&fl=key,title,edition_count,subject")?;
    assert_eq!(
        got["doc"],
        serde_json::json!({"key": "/works/OL1W", "title": "Alice II", "edition_count": 6, "subject": ["Fantasy"]})
    );
    assert_eq!(
        select(&[("q", "title:alice"), ("fq", "type:work")])?["response"]["numFound"],
        1
    );
    let both = get("ids=/works/OL2W,/works/NOPE,/works/OL1W&fl=key")?;
    assert_eq!(keys(&both), ["/works/OL2W", "/works/OL1W"]);
    assert_eq!(get("id=/works/NOPE")?["doc"], Value::Null);

    let missing = update(
        r#"{"add": {"doc": {"key": "/works/NOPE", "title": {"set": "x"}}}}"#,
        "",
    )?;
    assert_eq!(missing["responseHeader"]["status"], 400);
    assert_eq!(
        missing["error"]["msg"],
        "Document not found for update.  id=/works/NOPE"
    );
    let tolerant = update(
        r#"{"add": {"doc": {"title": "no key"}}, "add": {"doc": {"key": "/works/OL3W", "type": "work"}}}"#,
        "?update.chain=tolerant-chain",
    )?;
    assert_eq!(tolerant["responseHeader"]["status"], 0);
    assert_eq!(
        tolerant["responseHeader"]["errors"][0]["message"],
        "Document is missing mandatory uniqueKey field: key"
    );

    let deleted = update(
        r#"{"delete": ["/works/OL3W"], "delete": {"query": "key:\"/works/OL2W\""}, "commit": {}}"#,
        "",
    )?;
    assert_eq!(deleted["responseHeader"]["status"], 0);
    let export = request(
        &addr,
        "GET /solr/openlibrary/export?q=*:*&fl=key&sort=key%20asc HTTP/1.1",
        "",
    )?;
    assert_eq!(
        keys(&export),
        ["/authors/OL1A", "/books/OL1M", "/books/OL2M", "/works/OL1W"]
    );
    let gone = update(r#"{"delete": {"id": "/works/OL1W"}}"#, "")?;
    assert_eq!(gone["responseHeader"]["status"], 0);
    assert_eq!(
        select(&[("q", "*:*")])?["response"]["numFound"],
        1,
        "children go with their root"
    );
    let cores = request(&addr, "GET /solr/admin/cores?action=STATUS HTTP/1.1", "")?;
    assert_eq!(cores["status"]["openlibrary"]["index"]["numDocs"], 1);

    // A second server on the same directory sees what the first one saved.
    let restarted = start(&dir)?;
    let again = request(
        &restarted,
        "GET /solr/openlibrary/select?q=*:*&fl=key,name HTTP/1.1",
        "",
    )?;
    assert_eq!(
        again["response"]["docs"],
        serde_json::json!([{"key": "/authors/OL1A", "name": "Lewis Carroll"}])
    );
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
