extern crate chrono;
extern crate fastcgi;
extern crate hyper;
extern crate postgres;
extern crate regex;
extern crate rss;
extern crate scoped_threadpool;
extern crate tiny_keccak;
extern crate uuid;

use std::io;
use std::io::Write;

type UTCDateTime = chrono::DateTime<chrono::UTC>;

struct Page {
    name: String,
    url: String,

    last_modified: Option<UTCDateTime>,
    last_error: Option<String>,
    item_id: Option<uuid::Uuid>,

    http_etag: Option<String>,
    http_body_hash: Option<Vec<u8>>,

    delete_regex: Option<String>,

    // Other fields only used in SQL queries
}

// ----------------------------------------------------------------------------

fn main() {
    let conn = postgres::Connection::connect(
        database_url().as_ref(), postgres::TlsMode::None).unwrap();

    fastcgi::run(|mut req| {
        if Some("GET") != req.param("REQUEST_METHOD").as_ref().map(String::as_ref) {
            let _ = req.stdout().write(b"Status: 405 Method Not Allowed");
            return;
        }
        handle_request(&mut req, &conn).unwrap_or_else(|err| {
            let msg = format!("{:?}", err);
            let _ = req.stderr().write(msg.as_bytes());
            panic!("{}", msg);
        })
    })
}

fn database_url() -> String {
    std::env::args().nth(1).unwrap_or_else(|| {
        let user = std::env::var("USER").unwrap();
        format!("postgres://{}@%2Frun%2Fpostgresql/pagefeed", user)
    })
}

fn handle_request(req: &mut fastcgi::Request,
                  conn: &postgres::GenericConnection)
                  -> Result<(), PagefeedError> {
    let filter = get_pathinfo(req).trim_matches('/').replace('/', ".");

    let trans = try!(conn.transaction());
    try!(process_unchecked_pages(&trans, &filter));
    let pages = try!(get_pages(&trans, &filter));
    trans.set_commit();

    let response = build_feed(&pages);
    try!(req.stdout().write(b"Content-Type: application/rss+xml\n\n"));
    try!(response.write_to(req.stdout()));
    Ok(())
}

fn get_pathinfo(req: &fastcgi::Request) -> String {
    let request_uri = req.param("REQUEST_URI").unwrap_or("".into());
    let script_name = req.param("SCRIPT_NAME").unwrap_or("".into());
    if request_uri.starts_with(&script_name) {
        request_uri[script_name.len()..].into()
    } else {
        request_uri
    }
}


// ----------------------------------------------------------------------------

#[derive(Debug)]
enum PagefeedError {
    Io(io::Error),
    Postgres(postgres::error::Error),
    PostgresConnection(postgres::error::ConnectError),
    Hyper(hyper::Error),
    Regex(regex::Error),
    Rss(rss::Error),
}

impl From<io::Error> for PagefeedError {
    fn from(err: io::Error) -> PagefeedError {
        PagefeedError::Io(err)
    }
}

impl From<postgres::error::Error> for PagefeedError {
    fn from(err: postgres::error::Error) -> PagefeedError {
        PagefeedError::Postgres(err)
    }
}

impl From<postgres::error::ConnectError> for PagefeedError {
    fn from(err: postgres::error::ConnectError) -> PagefeedError {
        PagefeedError::PostgresConnection(err)
    }
}

impl From<hyper::Error> for PagefeedError {
    fn from(err: hyper::Error) -> PagefeedError {
        PagefeedError::Hyper(err)
    }
}

impl From<regex::Error> for PagefeedError {
    fn from(err: regex::Error) -> PagefeedError {
        PagefeedError::Regex(err)
    }
}

impl From<rss::Error> for PagefeedError {
    fn from(err: rss::Error) -> PagefeedError {
        PagefeedError::Rss(err)
    }
}

// ----------------------------------------------------------------------------

fn build_feed(pages: &Vec<Page>) -> rss::Channel {
    let items = pages.iter().filter(|page| {
        page.last_modified.is_some()
    }).map(|page| {
        rss::Item {
            title: Some(page.name.to_owned()),
            description: Some(describe_page_status(page)),
            link: Some(page.url.to_owned()),
            pub_date: Some(page.last_modified.unwrap().to_rfc2822()),
            guid: Some(rss::Guid {
                is_permalink: false,
                value: format!("{}", page.item_id.unwrap().urn()),
            }),
            ..Default::default()
        }
    }).collect();

    rss::Channel {
        title: String::from("Pagefeed"),
        link: String::from("urn:x-pagefeed:nowhere"),
        description: String::from("Pagefeed checks web pages for updates."),
        items: items,
        ..Default::default()
    }
}

fn describe_page_status(page: &Page) -> String {
    page.last_error.as_ref().map_or_else(
        || format!("{} was updated.", page.name),
        |err| format!("Erorr while checking {}: {}", page.name, err))
}

// ----------------------------------------------------------------------------

#[derive(Clone)]
enum PageStatus {
    Unmodified,
    Modified { body_hash: Vec<u8>, etag: Option<String> },
    FetchError (String)
}

const POOL_SIZE : u32 = 5;

fn process_unchecked_pages(conn: &postgres::GenericConnection, filter: &str)
                           -> Result<(), postgres::error::Error> {
    let now = chrono::UTC::now();
    let pages = try!(get_unchecked_pages(conn, filter));
    let statuses = check_pages(&pages);
    for (page, status) in pages.iter().zip(statuses.iter()) {
        match *status {
            PageStatus::Unmodified =>
                try!(update_page_unchanged(conn, page, &now)),
            PageStatus::Modified { ref body_hash, ref etag } =>
                try!(update_page_changed(conn, page, &now, etag, body_hash)),
            PageStatus::FetchError(ref error) =>
                try!(update_page_error(conn, page, &now, error)),
        };
    }
    Ok(())
}

fn check_pages(pages: &Vec<Page>) -> Vec<PageStatus> {
    let mut results = Vec::new();
    results.resize(pages.len(), PageStatus::Unmodified);
    let mut pool = scoped_threadpool::Pool::new(POOL_SIZE);
    pool.scoped(|scoped| {
        for (idx, result) in results.iter_mut().enumerate() {
            scoped.execute(move || {
                *result = check_page(&pages[idx]);
            });
        }
    });
    results
}

fn check_page(page: &Page) -> PageStatus {
    use hyper::client::{Client, RedirectPolicy};
    use hyper::header;
    use hyper::status::StatusCode;
    use std::str::FromStr;

    let mut headers = header::Headers::new();
    headers.set(header::UserAgent("Mozilla/5.0".to_owned()));
    if let Some(ref etag) = page.http_etag {
        if let Ok(etag) = header::EntityTag::from_str(etag) {
            headers.set(header::IfNoneMatch::Items(vec![etag]));
        }
    }

    let mut client = Client::new();
    client.set_redirect_policy(RedirectPolicy::FollowAll);

    let status = client.get(&page.url).headers(headers).send()
        .map_err(PagefeedError::from)
        .and_then(|mut response| {
            if response.status == StatusCode::NotModified {
                Ok(PageStatus::Unmodified)
            } else {
                let etag = extract_etag(&response);
                let hash = try!(hash(page, &mut response));
                Ok(PageStatus::Modified { body_hash: hash, etag: etag })
            }
        }).unwrap_or_else(|err| {
            PageStatus::FetchError(format!("{:?}", err))
        });

    match status {
        PageStatus::Modified { ref body_hash, .. }
        if Some(body_hash) == page.http_body_hash.as_ref() =>
            PageStatus::Unmodified,

        PageStatus::FetchError(ref error)
        if Some(error) == page.last_error.as_ref() =>
            PageStatus::Unmodified,

        _ => status
    }
}

fn extract_etag(r: &hyper::client::response::Response) -> Option<String> {
    r.headers.get().map(|&hyper::header::ETag(ref etag)| {
        format!("{}", etag)
    })
}

// ----------------------------------------------------------------------------

fn hash(page: &Page, r: &mut io::Read) -> Result<Vec<u8>, PagefeedError> {
    let mut buf = Vec::new();
    try!(r.read_to_end(&mut buf));

    if let Some(delete_regex) = page.delete_regex.as_ref() {
        let re = try!(regex::bytes::Regex::new(delete_regex));
        buf = re.replace_all(&buf, &b""[..]).into_owned();
    }

    let mut sha3 = tiny_keccak::Keccak::new_sha3_256();
    sha3.update(&buf);
    let mut res: [u8; 32] = [0; 32];
    sha3.finalize(&mut res);
    Ok(res.to_vec())
}

// ----------------------------------------------------------------------------

fn get_pages(conn: &postgres::GenericConnection, filter: &str)
                 -> Result<Vec<Page>, postgres::error::Error> {
    let query = "
select name, url, last_modified, last_error, item_id, http_etag, http_body_hash, delete_regex
from pages
where category <@ $1::text::ltree
";
    conn.query(query, &[&filter]).map(|rows| {
        rows.iter().map(instantiate_page).collect()
    })
}

fn get_unchecked_pages(conn: &postgres::GenericConnection, filter: &str)
                       -> Result<Vec<Page>, postgres::error::Error> {
    let query = "
select name, url, last_modified, last_error, item_id, http_etag, http_body_hash, delete_regex
from pages
where category <@ $1::text::ltree
and (last_checked is null
  or current_timestamp > greatest(
    last_checked + check_interval,
    last_modified + cooldown))
and enabled
for update
";
    conn.query(query, &[&filter]).map(|rows| {
        rows.iter().map(instantiate_page).collect()
    })
}

fn instantiate_page(row: postgres::rows::Row) -> Page {
    Page {
        name: row.get("name"),
        url: row.get("url"),
        last_modified: row.get("last_modified"),
        last_error: row.get("last_error"),
        item_id: row.get("item_id"),
        http_etag: row.get("http_etag"),
        http_body_hash: row.get("http_body_hash"),
        delete_regex: row.get("delete_regex"),
    }
}

fn update_page_unchanged(conn: &postgres::GenericConnection, page: &Page,
                         dt: &UTCDateTime)
                         -> Result<(), postgres::error::Error> {
    let query = "
update pages
set last_checked = $1
where name = $2
";
    try!(conn.execute(query, &[dt, &page.name]));
    Ok(())
}

fn update_page_changed(conn: &postgres::GenericConnection, page: &Page,
                       dt: &UTCDateTime, new_etag: &Option<String>, new_hash: &Vec<u8>)
                       -> Result<(), postgres::error::Error> {
    let query = "
update pages
set last_checked = $1,
    last_modified = $1,
    last_error = null,
    item_id = $2,
    http_etag = $3,
    http_body_hash = $4
where name = $5
";
    let uuid = uuid::Uuid::new_v4();
    try!(conn.execute(query, &[dt, &uuid, new_etag, new_hash, &page.name]));
    Ok(())
}

fn update_page_error(conn: &postgres::GenericConnection, page: &Page,
                     dt: &UTCDateTime, error: &String)
                     -> Result<(), postgres::error::Error> {
    let query = "
update pages
set last_checked = $1,
    last_modified = $1,
    last_error = $2,
    item_id = $3,
    http_etag = null,
    http_body_hash = null
where name = $4
";
    let uuid = uuid::Uuid::new_v4();
    try!(conn.execute(query, &[dt, error, &uuid, &page.name]));
    Ok(())
}

// ----------------------------------------------------------------------------
