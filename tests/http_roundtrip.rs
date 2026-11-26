//! End-to-end checks against the real HTTP server on a tiny data set.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::thread;
use std::time::Duration;

use minisolr_rs::analysis::{Analyzer, Chain};
use minisolr_rs::server::serve;
use minisolr_rs::store::Store;
use serde_json::Value;

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

#[test]
fn serves_select_ping_and_post_forms() -> TestResult {
    let store = Store::load(Path::new("tests/data/mini.jsonl"))?;
    let addr = free_addr()?;
    let bound = addr.clone();
    thread::spawn(move || serve(&store, &bound));
    thread::sleep(Duration::from_millis(300));

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
