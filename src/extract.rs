use crate::{Item, ItemBody, Mode, PageConfig};

type Error = Box<dyn std::error::Error>;

pub fn extract(pc: &PageConfig, document: String) -> Result<Vec<Item>, Error> {
    match &pc.mode {
        Mode::Text => extract_text(pc, document),
        Mode::Html => extract_singlehtml(pc, document),
        Mode::MultiHtml => extract_multihtml(pc, document),
        Mode::Json => extract_json(pc, document),
    }
}

fn extract_text(_pc: &PageConfig, document: String) -> Result<Vec<Item>, Error> {
    Ok(vec![Item {
        body: ItemBody::Text(document),
        title: None,
        url: None,
    }])
}

fn extract_singlehtml(pc: &PageConfig, document: String) -> Result<Vec<Item>, Error> {
    let parts = extract_multihtml(pc, document)?;
    if parts.is_empty() {
        return Ok(vec![]);
    }

    let mut body = String::new();
    let mut title = None;
    let mut url = None;
    for part in parts {
        if let ItemBody::Html(html) = part.body {
            body.push_str(&html);
        }
        title = title.or(part.title);
        url = url.or(part.url);
    }
    Ok(vec![Item {
        body: ItemBody::Html(body),
        title,
        url,
    }])
}

fn extract_multihtml(pc: &PageConfig, document: String) -> Result<Vec<Item>, Error> {
    use scraper::{Html, Selector};

    let default_item_sel = "body";
    let default_title_sel = "h1,h2,h3,h4,h5,h6";
    let default_text_sel = ":root";
    let default_url_sel = ":root";

    let item_sel = Selector::parse(pc.item_selector.as_deref().unwrap_or(default_item_sel))
        .expect("invalid item_selector");
    let title_sel = Selector::parse(pc.title_selector.as_deref().unwrap_or(default_title_sel))
        .expect("invalid title_selector");
    let text_sel = Selector::parse(pc.text_selector.as_deref().unwrap_or(default_text_sel))
        .expect("invalid text_selector");
    let url_sel = Selector::parse(pc.url_selector.as_deref().unwrap_or(default_url_sel))
        .expect("invalid url_selector");

    let document = Html::parse_document(&document);
    let mut result = vec![];
    for item_el in document.select(&item_sel) {
        let title = item_el
            .select(&title_sel)
            .next()
            .map(|el| el.text().collect::<String>());
        let body = ItemBody::Html(
            item_el
                .select(&text_sel)
                .next()
                .unwrap_or(item_el)
                .inner_html(),
        );
        let url_el = item_el.select(&url_sel).next().unwrap_or(item_el);
        let url_el_href = url_el.value().attr("href");
        let url_el_src = url_el.value().attr("src");
        let url = url_el_href.or(url_el_src).map(str::to_string);
        result.push(Item { title, body, url })
    }
    Ok(result)
}

fn extract_json(pc: &PageConfig, document: String) -> Result<Vec<Item>, Error> {
    use jaq_all::jaq_core::unwrap_valr;
    use jaq_all::{data, json, load};

    let text_key = "text".to_string().into();
    let title_key = "title".to_string().into();
    let url_key = "url".to_string().into();

    let jaq_source: &str = pc.jaq.as_deref().unwrap_or(r#"{"text": tostring}"#);
    let filter = data::compile(jaq_source).map_err(|frs| {
        frs.iter()
            .map(|fr| load::FileReportsDisp::new(fr).to_string())
            .collect::<Vec<String>>()
            .join("\n")
    })?;

    let input_json = json::read::parse_single(document.as_bytes());
    let input_filename = "<http-body>".to_string().into();
    let input = (&input_filename, std::iter::once(input_json));

    let runner = Default::default();
    let vars = Default::default();
    let fi = |e| Error::from(e);

    let mut result = vec![];

    data::run(&runner, &filter, vars, input, fi, |v| {
        let item = unwrap_valr(v).map_err(|e| e.to_string())?;

        let text = get_string_from_map(&item, &text_key).ok_or("text key missing")?;
        let title = get_string_from_map(&item, &title_key);
        let url = get_string_from_map(&item, &url_key);

        result.push(Item {
            body: ItemBody::Html(text),
            title,
            url,
        });
        Ok(())
    })?;

    Ok(result)
}

fn get_string_from_map(obj: &jaq_all::json::Val, key: &jaq_all::json::Val) -> Option<String> {
    use jaq_all::jaq_core::ValT as _;
    use jaq_all::jaq_std::ValT as _;
    let v = obj.clone().index(key).ok()?;
    Some(String::from_utf8_lossy(v.as_utf8_bytes()?).into_owned())
}
