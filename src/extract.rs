use crate::{Item, ItemBody, Mode, PageConfig};
use std::rc::Rc;

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

    let item_sel = Selector::parse(pc.item_selector.as_deref().unwrap_or(&default_item_sel))
        .expect("invalid item_selector");
    let title_sel = Selector::parse(pc.title_selector.as_deref().unwrap_or(&default_title_sel))
        .expect("invalid title_selector");
    let text_sel = Selector::parse(pc.text_selector.as_deref().unwrap_or(&default_text_sel))
        .expect("invalid text_selector");

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
        result.push(Item {
            title,
            body,
            url: None,
        })
    }
    Ok(result)
}

fn extract_json(pc: &PageConfig, document: String) -> Result<Vec<Item>, Error> {
    use jaq_interpret::{Ctx, Error, FilterT, RcIter, Val};
    use serde_json::Value;

    let jaq_program: &str = match &pc.jaq {
        Some(filter) => &filter,
        None => &r#"{"text": tostring}"#,
    };
    let filter = compile_jaq(jaq_program)?;

    let text_key = Rc::new("text".to_string());
    let title_key = Rc::new("title".to_string());
    let url_key = Rc::new("url".to_string());

    let json: Value = serde_json::from_str(&document)?;
    let mut result = vec![];

    let inputs = RcIter::new(core::iter::empty());
    for item in filter.run((Ctx::new([], &inputs), Val::from(json))) {
        let item = item?;
        let text = get_string_from_map(&item, &text_key)?
            .ok_or_else(|| Error::Index(item.clone(), Val::str(text_key.to_string())))?;
        let title = get_string_from_map(&item, &title_key)?;
        let url = get_string_from_map(&item, &url_key)?;

        result.push(Item {
            body: ItemBody::Html(text),
            title,
            url,
        });
    }

    Ok(result)
}

fn get_string_from_map(
    obj: &jaq_interpret::Val,
    key: &Rc<String>,
) -> Result<Option<String>, jaq_interpret::Error> {
    use jaq_interpret::{Error, Val};

    let Val::Obj(map) = obj else {
        return Err(Error::Index(obj.clone(), Val::str(key.to_string())));
    };
    match (*map).get(key) {
        None => Ok(None),
        Some(value) => Ok(Some(value.clone().to_str()?.to_string())),
    }
}

fn compile_jaq(filter: &str) -> Result<jaq_interpret::Filter, Error> {
    let mut defs = jaq_interpret::ParseCtx::new(Vec::new());
    defs.insert_defs(jaq_std::std());

    let (f, errs) = jaq_parse::parse(filter, jaq_parse::main());
    if !errs.is_empty() {
        panic!("Failed to parse: {}", filter);
    }

    let f = defs.compile(f.unwrap());
    if !defs.errs.is_empty() {
        panic!("Failed to parse: {}", filter);
    }

    Ok(f)
}
