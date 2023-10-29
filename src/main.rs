use futures::stream::futures_unordered::FuturesUnordered;
use futures::StreamExt;
use maud::html;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod extract;

#[derive(Deserialize)]
struct Config {
    output_dir: PathBuf,
    state_file: PathBuf,

    #[serde(flatten)]
    pages: HashMap<String, PageConfig>,
}

#[derive(Deserialize)]
pub struct PageConfig {
    name: String,
    url: String,
    post_json: Option<String>,

    #[serde(default = "default_interval")]
    #[serde(with = "humantime_serde")]
    interval: Duration,

    #[serde(default)]
    #[serde(with = "humantime_serde")]
    cooldown: Duration,

    mode: Mode,

    // HTML options
    item_selector: Option<String>,
    text_selector: Option<String>,
    title_selector: Option<String>,
    url_selector: Option<String>,

    // JSON options
    jaq: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Text,
    Html,
    MultiHtml,
    Json,
}

fn default_interval() -> Duration {
    Duration::from_secs(7200)
}

#[derive(Serialize, Deserialize)]
struct PageState {
    error: Option<String>,

    #[serde(with = "time::serde::rfc3339")]
    last_modified: time::OffsetDateTime,

    #[serde(with = "time::serde::rfc3339")]
    last_checked: time::OffsetDateTime,

    http_etag: Option<String>,

    items: Vec<Item>,
}

#[derive(Eq, PartialEq, Serialize, Deserialize)]
pub struct Item {
    url: Option<String>,
    title: Option<String>,
    body: ItemBody,
}

#[derive(Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ItemBody {
    Text(String),
    Html(String),
}

impl Default for PageState {
    fn default() -> Self {
        let now = time::OffsetDateTime::now_utc();
        Self {
            error: None,
            last_modified: now,
            last_checked: now,
            http_etag: None,
            items: vec![],
        }
    }
}

impl PageState {
    fn failure(self, now: time::OffsetDateTime, error: String) -> Self {
        Self {
            error: Some(error),
            last_checked: now,
            http_etag: None,
            ..self
        }
    }

    fn not_modified(self, now: time::OffsetDateTime) -> Self {
        Self {
            error: None,
            last_checked: now,
            ..self
        }
    }

    fn update_content(
        self,
        now: time::OffsetDateTime,
        http_etag: Option<String>,
        items: Vec<Item>,
    ) -> Self {
        if items == self.items {
            self.not_modified(now)
        } else {
            Self {
                error: None,
                last_modified: now,
                last_checked: now,
                http_etag,
                items,
            }
        }
    }
}

async fn update_pages(
    configs: &HashMap<String, PageConfig>,
    mut states: HashMap<String, PageState>,
) -> HashMap<String, PageState> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .build()
        .unwrap();

    configs
        .iter()
        .map(|(slug, config)| {
            let state = states.remove(slug);
            async {
                let updated_state = match state {
                    None => fetch_page(&client, config, PageState::default()).await,
                    Some(state) => {
                        if is_time_to_fetch(config, &state) {
                            fetch_page(&client, config, state).await
                        } else {
                            state
                        }
                    }
                };
                (slug.clone(), updated_state)
            }
        })
        .collect::<FuturesUnordered<_>>()
        .collect::<HashMap<_, _>>()
        .await
}

fn is_time_to_fetch(page: &PageConfig, state: &PageState) -> bool {
    time::OffsetDateTime::now_utc() - Duration::from_secs(60)
        > std::cmp::max(
            state.last_checked + page.interval,
            state.last_modified + page.cooldown,
        )
}

async fn fetch_page(client: &reqwest::Client, page: &PageConfig, state: PageState) -> PageState {
    let now = time::OffsetDateTime::now_utc();

    let mut request;
    if let Some(ref post_json) = page.post_json {
        request = client
            .post(&page.url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/json".to_string(),
            )
            .body(post_json.to_string());
    } else {
        request = client.get(&page.url);
        if let Some(ref etag) = state.http_etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag.clone());
        }
    }

    let response = match request.send().await {
        Err(error) => return state.failure(now, format!("{:?}", error)),
        Ok(response) if !response.status().is_success() => {
            return state.failure(now, format!("HTTP status {}", response.status()))
        }
        Ok(response) if response.status() == reqwest::StatusCode::NOT_MODIFIED => {
            return state.not_modified(now)
        }
        Ok(response) => response,
    };

    let etag_header = get_header(&response, reqwest::header::ETAG);
    let document = match response.text().await {
        Err(error) => return state.failure(now, format!("{:?}", error)),
        Ok(document) => document,
    };

    let items = match extract::extract(page, document) {
        Err(error) => return state.failure(now, format!("{:?}", error)),
        Ok(items) => items,
    };
    state.update_content(now, etag_header, items)
}

fn get_header(response: &reqwest::Response, header: reqwest::header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(header)
        .and_then(|x| x.to_str().ok())
        .map(str::to_string)
}

fn item_uuid(content: &Item) -> uuid::Uuid {
    use uuid::{uuid, Uuid};
    const NAMESPACE: Uuid = uuid!("846a3c88-0db8-11ee-a20b-74d435e57678");
    let bytes = toml::to_string(&content).unwrap().into_bytes();
    Uuid::new_v5(&NAMESPACE, &bytes)
}

fn build_rss(page: &PageConfig, state: &PageState) -> rss::Channel {
    let mut items: Vec<rss::Item> = vec![];

    if let Some(error) = &state.error {
        items.push(
            rss::ItemBuilder::default()
                .title("Error".to_owned())
                .link(page.url.clone())
                .description(error_to_html(error))
                .build(),
        )
    }

    for c in &state.items {
        items.push(
            rss::ItemBuilder::default()
                .title(c.title.as_ref().unwrap_or(&page.name).clone())
                .link(c.url.as_ref().unwrap_or(&page.url).clone())
                .description(match &c.body {
                    ItemBody::Html(t) => t.clone(),
                    ItemBody::Text(t) => text_to_html(t),
                })
                .guid(
                    rss::GuidBuilder::default()
                        .value(item_uuid(c).as_urn().to_string())
                        .permalink(false)
                        .build(),
                )
                .build(),
        )
    }
    if state.items.is_empty() {
        items.push(
            rss::ItemBuilder::default()
                .title(page.name.clone())
                .link(page.url.clone())
                .description("No items found!".to_string())
                .guid(
                    rss::GuidBuilder::default()
                        .value(format!("empty:{}", page.name))
                        .permalink(false)
                        .build(),
                )
                .build(),
        )
    }

    rss::ChannelBuilder::default()
        .title(page.name.clone())
        .link(page.url.clone())
        .items(items)
        .build()
}

fn text_to_html(text: &str) -> String {
    html! { pre { (text) } }.into_string()
}

fn error_to_html(error: &str) -> String {
    html! { p { code { (error) } } }.into_string()
}

fn build_index(config: &Config) -> String {
    html! {
        html {
            head {
                title { "Pagefeed index" }
                @for (slug, page_config) in &config.pages {
                    link rel="alternative"
                        type="application/rss+xml"
                        title=(page_config.name)
                        href=(format!("{slug}.xml"));
                }
            }
            body {}
        }
    }
    .into_string()
}

fn write_unless_unmodified(path: &Path, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{Read, Seek, Write};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?;
    let unmodified = {
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer).is_ok() && buffer == data
    };
    if !unmodified {
        file.rewind()?;
        file.set_len(0)?;
        file.write_all(data)?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let config_file = std::env::args().nth(1).expect("no config file given");
    let base_path = Path::new(&config_file).parent().unwrap();
    let config: Config = toml::from_str(&std::fs::read_to_string(&config_file)?)?;

    let state_file = base_path.join(&config.state_file);
    let state: HashMap<String, PageState> = std::fs::read_to_string(&state_file)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();

    let state = update_pages(&config.pages, state).await;

    let af = atomicwrites::AtomicFile::new(&state_file, atomicwrites::AllowOverwrite);
    let state_data = toml::to_string(&state)?.into_bytes();
    af.write(|f| f.write_all(&state_data))?;
    drop(af);

    let output_dir = base_path.join(&config.output_dir);
    for (slug, page_config) in &config.pages {
        let page_state = state.get(slug).unwrap();
        let rss = build_rss(page_config, page_state);
        write_unless_unmodified(
            &output_dir.join(format!("{slug}.xml")),
            rss.to_string().as_bytes(),
        )?;
    }
    write_unless_unmodified(
        &output_dir.join("index.html"),
        build_index(&config).as_bytes(),
    )?;

    Ok(())
}
