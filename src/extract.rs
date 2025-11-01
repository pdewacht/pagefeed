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
    use jaq_json::Val;
    use serde_json::Value;

    let text_key = Val::from("text".to_string());
    let title_key = Val::from("title".to_string());
    let url_key = Val::from("url".to_string());

    let jaq_source: &str = match &pc.jaq {
        Some(filter) => filter,
        None => r#"{"text": tostring}"#,
    };

    let json: Value = serde_json::from_str(&document)?;

    let mut result = vec![];
    for item in run_jaq(jaq_source, json)? {
        let item = item.map_err(|e| e.to_string())?;

        let text = get_string_from_map(&item, &text_key).ok_or("text key missing")?;
        let title = get_string_from_map(&item, &title_key);
        let url = get_string_from_map(&item, &url_key);

        result.push(Item {
            body: ItemBody::Html(text),
            title,
            url,
        });
    }

    Ok(result)
}

fn get_string_from_map(obj: &jaq_json::Val, key: &jaq_json::Val) -> Option<String> {
    use jaq_core::ValT;
    Some(obj.clone().index(key).ok()?.as_str()?.to_string())
}

fn run_jaq(
    source: &str,
    input: serde_json::Value,
) -> Result<Vec<jaq_core::ValR<jaq_json::Val>>, Error> {
    use jaq_core::load::{Arena, File, Loader};
    use jaq_core::{Compiler, Ctx, RcIter};
    use jaq_json::Val;

    let loader = Loader::new(jaq_std::defs().chain(jaq_json::defs()));
    let arena = Arena::default();
    let modules = loader
        .load(
            &arena,
            File {
                code: source,
                path: (),
            },
        )
        .unwrap();

    let filter = Compiler::default()
        .with_funs(jaq_std::funs().chain(jaq_json::funs()))
        .compile(modules)
        .unwrap();

    let inputs = RcIter::new(core::iter::empty());
    let out = filter.run((Ctx::new([], &inputs), Val::from(input)));
    Ok(out.take(100).collect())
}
