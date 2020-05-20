extern crate chrono;
extern crate fastcgi;
extern crate htmlescape;
extern crate minidom;
extern crate pg_interval;
extern crate postgres;
extern crate regex;
extern crate reqwest;
extern crate rss;
extern crate tiny_keccak;
extern crate uuid;

use std::io;
use std::io::Write;

type UtcDateTime = chrono::DateTime<chrono::Utc>;

struct Page {
    slug: String,
    name: String,
    url: String,
    check_interval: chrono::Duration,
    cooldown: chrono::Duration,
    //enabled: bool,

    last_checked: Option<UtcDateTime>,
    last_modified: Option<UtcDateTime>,
    last_error: Option<String>,
    item_id: Option<uuid::Uuid>,

    http_etag: Option<String>,
    http_body_hash: Option<Vec<u8>>,

    delete_regex: Option<String>,
}

// ----------------------------------------------------------------------------

fn main() {
    fastcgi::run(|mut req| {
        if Some("GET") != req.param("REQUEST_METHOD").as_ref().map(String::as_ref) {
            let _ = req.stdout().write(b"Status: 405 Method Not Allowed\n");
            return;
        }
        handle_request(&mut req).unwrap_or_else(|err| {
            let msg = format!("{:?}", err);
            let _ = req.stderr().write(msg.as_bytes());
            panic!("{}", msg);
        })
    })
}

fn database_connection() -> Result<postgres::Client, PagefeedError> {
    let connection = postgres::Client::connect(
        database_url().as_ref(), postgres::NoTls)?;
    Ok(connection)
}

fn database_url() -> String {
    std::env::args().nth(1).unwrap_or_else(|| {
        let user = std::env::var("USER").unwrap();
        format!("postgres://{}@%2Frun%2Fpostgresql/pagefeed", user)
    })
}

fn handle_request(req: &mut fastcgi::Request)
                  -> Result<(), PagefeedError> {
    let url = get_url(req)?;
    let pathinfo = get_pathinfo(req);
    let slug = pathinfo.trim_matches('/');

    let mut w = io::BufWriter::new(req.stdout());
    if slug == "" {
        handle_opml_request(&url, &mut w)
    } else {
        handle_feed_request(slug, &mut w)
    }
}

fn handle_opml_request<W: Write>(url: &str, out: &mut W) -> Result<(), PagefeedError> {
    let mut conn = database_connection()?;
    let mut trans = conn.transaction()?;
    let pages = get_enabled_pages(&mut trans)?;
    trans.commit()?;
    out.write(b"Content-Type: application/xml\n\n")?;
    build_opml(url, &pages).write_to(out)?;
    Ok(())
}

fn handle_feed_request<W: Write>(slug: &str, out: &mut W) -> Result<(), PagefeedError> {
    let mut conn = database_connection()?;
    let mut trans = conn.transaction()?;
    let page = get_page(&mut trans, &slug)?;
    let page = page.map(|page| refresh_page(&mut trans, page)).transpose()?;
    trans.commit()?;

    match page {
        None => {
            out.write(b"Status: 404 Not Found\n")?;
            Ok(())
        },
        Some(page) => {
            let feed = build_feed(&page)?;
            out.write(b"Content-Type: application/rss+xml\n\n")?;
            feed.write_to(out)?;
            Ok(())
        }
    }
}

fn get_url(req: &fastcgi::Request) -> Result<String, PagefeedError> {
    use std::io::{Error, ErrorKind};

    let https = match req.param("HTTPS") {
        Some(ref s) => s == "on",
        _ => false,
    };

    let server_addr = req.param("SERVER_ADDR")
        .ok_or(Error::new(ErrorKind::Other, "SERVER_ADDR unset"))?;
    let server_port = req.param("SERVER_PORT")
        .ok_or(Error::new(ErrorKind::Other, "SERVER_PORT unset"))?
        .parse::<u16>()
        .or(Err(Error::new(ErrorKind::Other, "SERVER_PORT invalid")))?;

    let mut script_name = req.param("SCRIPT_NAME")
        .ok_or(Error::new(ErrorKind::Other, "SCRIPT_NAME unset"))?;
    if !script_name.starts_with('/') {
        script_name.insert(0, '/')
    }
    if !script_name.ends_with('/') {
        script_name.push('/')
    }

    Ok(match (https, server_port) {
        (false, 80) => format!("http://{}{}", server_addr, script_name),
        (false, _)  => format!("http://{}:{}{}", server_addr, server_port, script_name),
        (true, 443) => format!("https://{}{}", server_addr, script_name),
        (true, _)   => format!("https://{}:{}{}", server_addr, server_port, script_name),
    })
}

fn get_pathinfo(req: &fastcgi::Request) -> String {
    req.param("PATH_INFO").unwrap_or("".to_string())
}


// ----------------------------------------------------------------------------

#[derive(Debug)]
enum PagefeedError {
    Io(io::Error),
    Postgres(postgres::error::Error),
    Regex(regex::Error),
    Reqwest(reqwest::Error),
    Rss(rss::Error),
    RssBuilder(String),
    Minidom(minidom::Error),
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

impl From<regex::Error> for PagefeedError {
    fn from(err: regex::Error) -> PagefeedError {
        PagefeedError::Regex(err)
    }
}

impl From<reqwest::Error> for PagefeedError {
    fn from(err: reqwest::Error) -> PagefeedError {
        PagefeedError::Reqwest(err)
    }
}

impl From<rss::Error> for PagefeedError {
    fn from(err: rss::Error) -> PagefeedError {
        PagefeedError::Rss(err)
    }
}

impl From<minidom::Error> for PagefeedError {
    fn from(err: minidom::Error) -> PagefeedError {
        PagefeedError::Minidom(err)
    }
}

// ----------------------------------------------------------------------------

fn build_feed(page: &Page) -> Result<rss::Channel, PagefeedError> {
    let mut items = vec!();

    if page.last_modified.is_some() {
        let guid = rss::GuidBuilder::default()
            .value(format!("{}", page.item_id.unwrap().to_urn()))
            .permalink(false)
            .build()
            .map_err(PagefeedError::RssBuilder)?;

        let item = rss::ItemBuilder::default()
            .title(page.name.to_owned())
            .description(describe_page_status(page))
            .link(page.url.to_owned())
            .pub_date(page.last_modified.unwrap().to_rfc2822())
            .guid(guid)
            .build()
            .map_err(PagefeedError::RssBuilder)?;

        items.push(item);
    }

    rss::ChannelBuilder::default()
        .title(page.name.to_owned())
        .link(page.url.to_owned())
        .items(items)
        .build()
        .map_err(PagefeedError::RssBuilder)
}

fn describe_page_status(page: &Page) -> String {
    page.last_error.as_ref().map_or_else(
        || format!("{} was updated.", page.name),
        |err| format!("Error while checking {}: {}", page.name, err))
}

fn build_opml(url: &str, pages: &Vec<Page>) -> minidom::Element {
    use minidom::Element;
    let head = Element::bare("head");
    let mut body = Element::bare("body");
    for page in pages {
        body.append_child(
            Element::builder("outline")
                .attr("type", "rss")
                .attr("text", htmlescape::encode_minimal(&page.name))
                .attr("xmlUrl", format!("{}{}", url, page.slug))
                .attr("htmlUrl", page.url.to_owned())
                .build());
    }
    Element::builder("opml")
        .attr("version", "2.0")
        .append(head)
        .append(body)
        .build()
}

// ----------------------------------------------------------------------------

#[derive(Clone)]
enum PageStatus {
    Unmodified,
    Modified { body_hash: Vec<u8>, etag: Option<String> },
    FetchError (String)
}

fn refresh_page(conn: &mut postgres::Transaction, page: Page)
                -> Result<Page, postgres::error::Error> {
    if !page_needs_checking(&page) {
        return Ok(page);
    }

    let status = check_page(&page);
    match status {
        PageStatus::Unmodified =>
            update_page_unchanged(conn, &page),
        PageStatus::Modified { ref body_hash, ref etag } =>
            update_page_changed(conn, &page, etag, body_hash),
        PageStatus::FetchError(ref error) =>
            update_page_error(conn, &page, error),
    }
}

fn page_needs_checking(page: &Page) -> bool {
    page.last_checked.is_none() ||
        chrono::Utc::now() >= std::cmp::max(
            page.last_checked.unwrap() + page.check_interval,
            page.last_modified.unwrap() + page.cooldown)
}

fn check_page(page: &Page) -> PageStatus {
    use reqwest::header;
    use reqwest::StatusCode;

    let client = reqwest::blocking::Client::new();
    let mut request = client.get(&page.url)
        .header(header::USER_AGENT, "Mozilla/5.0");

    if let Some(ref etag) = page.http_etag {
        request = request.header(header::IF_NONE_MATCH, etag.to_string());
    }

    let status = request.send()
        .map_err(PagefeedError::from)
        .and_then(|mut response| {
            if response.status() == StatusCode::NOT_MODIFIED {
                Ok(PageStatus::Unmodified)
            } else {
                let etag = response.headers().get(header::ETAG)
                    .and_then(|x| x.to_str().ok()).map(str::to_string);
                let hash = hash(page, &mut response)?;
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

// ----------------------------------------------------------------------------

fn hash(page: &Page, r: &mut dyn io::Read) -> Result<Vec<u8>, PagefeedError> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf)?;

    if let Some(delete_regex) = page.delete_regex.as_ref() {
        let re = regex::bytes::Regex::new(delete_regex)?;
        buf = re.replace_all(&buf, &b""[..]).into_owned();
    }

    let mut sha3 = tiny_keccak::Keccak::new_sha3_256();
    sha3.update(&buf);
    let mut res: [u8; 32] = [0; 32];
    sha3.finalize(&mut res);
    Ok(res.to_vec())
}

// ----------------------------------------------------------------------------

fn get_enabled_pages(conn: &mut postgres::Transaction)
                     -> Result<Vec<Page>, postgres::error::Error> {
    let query = "
select *
from pages
where enabled
";
    conn.query(query, &[]).map(|rows| {
        rows.iter().map(instantiate_page).collect()
    })
}


fn get_page(conn: &mut postgres::Transaction, slug: &str)
            -> Result<Option<Page>, postgres::error::Error> {
    let query = "
select *
from pages
where slug = $1
";
    conn.query(query, &[&slug]).map(|rows| {
        rows.iter().nth(0).map(instantiate_page)
    })
}

fn instantiate_page(row: &postgres::row::Row) -> Page {
    Page {
        slug: row.get("slug"),
        name: row.get("name"),
        url: row.get("url"),
        //enabled: row.get("enabled"),
        delete_regex: row.get("delete_regex"),
        check_interval: to_duration(row.get("check_interval")),
        cooldown: to_duration(row.get("cooldown")),
        last_checked: row.get("last_checked"),
        last_modified: row.get("last_modified"),
        last_error: row.get("last_error"),
        item_id: row.get("item_id"),
        http_etag: row.get("http_etag"),
        http_body_hash: row.get("http_body_hash"),
    }
}

fn to_duration(i: pg_interval::Interval) -> chrono::Duration {
    chrono::Duration::microseconds(i.microseconds) +
        chrono::Duration::days(i.days as i64) +
        chrono::Duration::days(i.months as i64 * 30)
}

fn update_page_unchanged(conn: &mut postgres::Transaction, page: &Page)
                         -> Result<Page, postgres::error::Error> {
    let query = "
update pages
set last_checked = current_timestamp
where slug = $1
returning *
";
    conn.query(query, &[&page.slug]).map(|rows| {
        rows.iter().nth(0).map(instantiate_page).unwrap()
    })
}

fn update_page_changed(conn: &mut postgres::Transaction, page: &Page,
                       new_etag: &Option<String>, new_hash: &Vec<u8>)
                       -> Result<Page, postgres::error::Error> {
    let query = "
update pages
set last_checked = current_timestamp,
    last_modified = current_timestamp,
    last_error = null,
    item_id = $1,
    http_etag = $2,
    http_body_hash = $3
where slug = $4
returning *
";
    let uuid = uuid::Uuid::new_v4();
    conn.query(query, &[&uuid, new_etag, new_hash, &page.slug]).map(|rows| {
        rows.iter().nth(0).map(instantiate_page).unwrap()
    })
}

fn update_page_error(conn: &mut postgres::Transaction, page: &Page, error: &String)
                     -> Result<Page, postgres::error::Error> {
    let query = "
update pages
set last_checked = current_timestamp,
    last_modified = current_timestamp,
    last_error = $1,
    item_id = $2,
    http_etag = null,
    http_body_hash = null
where slug = $3
returning *
";
    let uuid = uuid::Uuid::new_v4();
    conn.query(query, &[error, &uuid, &page.slug]).map(|rows| {
        rows.iter().nth(0).map(instantiate_page).unwrap()
    })
}

// ----------------------------------------------------------------------------
