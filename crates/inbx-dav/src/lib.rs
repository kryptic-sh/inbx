//! Shared DAV (CalDAV/CardDAV) PROPFIND + XML scrape helpers.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reqwest: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("server: {status}: {body}")]
    Server { status: u16, body: String },
}

pub type Result<T> = std::result::Result<T, Error>;

pub async fn propfind_raw(
    http: &reqwest::Client,
    url: &str,
    user: &str,
    password: &str,
    body: &str,
    depth: &str,
) -> Result<String> {
    let res = http
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), url)
        .basic_auth(user, Some(password))
        .header("Content-Type", "application/xml; charset=utf-8")
        .header("Depth", depth)
        .body(body.to_string())
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        return Err(Error::Server { status, body });
    }
    Ok(res.text().await?)
}

pub async fn propfind_extract(
    http: &reqwest::Client,
    url: &str,
    user: &str,
    password: &str,
    body: &str,
    depth: &str,
    parent_tag: &str,
) -> Result<Option<String>> {
    let xml = propfind_raw(http, url, user, password, body, depth).await?;
    Ok(find_href_under(&xml, parent_tag))
}

pub fn split_responses(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = 0;
    while let Some(start) = xml[cur..].find("<") {
        let abs_start = cur + start;
        let after = &xml[abs_start..];
        // Match `<response` or `<*:response`.
        let header_end = match after.find('>') {
            Some(e) => abs_start + e + 1,
            None => break,
        };
        let header = &xml[abs_start..header_end];
        if header.contains("response") && !header.contains("/>") && !header.contains("</") {
            let close_needle = match header
                .trim_start_matches('<')
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
            {
                Some(name) => format!("</{name}>"),
                None => break,
            };
            let Some(close_off) = xml[header_end..].find(&close_needle) else {
                break;
            };
            let abs_close = header_end + close_off + close_needle.len();
            out.push(decode_xml_entities(&xml[abs_start..abs_close]));
            cur = abs_close;
        } else {
            cur = header_end;
        }
    }
    out
}

pub fn find_href_under(xml: &str, parent_tag: &str) -> Option<String> {
    // Locate the parent tag (matching `<*:tag` or `<tag`), then the first
    // <href> inside it.
    let lower = xml.to_ascii_lowercase();
    let needle = parent_tag.to_ascii_lowercase();
    let start = lower.find(&needle)?;
    let end = lower[start..]
        .find(&format!("/{needle}"))
        .map(|e| start + e)
        .unwrap_or(xml.len());
    let slice = &xml[start..end];
    extract_tag_text(slice, "href")
}

pub fn extract_tag_text(xml: &str, tag: &str) -> Option<String> {
    // Match `<tag>text</tag>` or `<*:tag>text</*:tag>` case-insensitively.
    let lower = xml.to_ascii_lowercase();
    let mut cur = 0;
    let needle = format!("{tag}>");
    while let Some(off) = lower[cur..].find(&needle) {
        let open_end = cur + off + needle.len();
        // confirm preceding char is `<` or `:`
        let bytes = lower.as_bytes();
        if open_end < needle.len() + 1 {
            return None;
        }
        let preceding = bytes[cur + off - 1];
        if preceding != b'<' && preceding != b':' {
            cur = open_end;
            continue;
        }
        // Find closing tag.
        let close = lower[open_end..].find("</")?;
        let text = &xml[open_end..open_end + close];
        let text = decode_xml_entities(text.trim());
        return Some(text);
    }
    None
}

pub fn absolutize(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    // base looks like "https://host/path"; replace path with href if href starts with /.
    if let Some(scheme_end) = base.find("://") {
        let after = &base[scheme_end + 3..];
        let host_end = after
            .find('/')
            .map(|i| scheme_end + 3 + i)
            .unwrap_or(base.len());
        let host_part = &base[..host_end];
        if href.starts_with('/') {
            return format!("{host_part}{href}");
        }
        // relative href — append with `/`
        let trimmed = base.trim_end_matches('/');
        return format!("{trimmed}/{href}");
    }
    href.to_string()
}

pub fn decode_xml_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}
