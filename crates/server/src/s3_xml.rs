use time::{
    OffsetDateTime,
    format_description::well_known::{Rfc2822, Rfc3339},
};

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 16;
const MAX_ELEMENTS: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum XmlError {
    #[error("the XML document is too large")]
    TooLarge,
    #[error("the XML document is too deeply nested")]
    TooDeep,
    #[error("the XML document is malformed")]
    Malformed,
    #[error("XML document types are not accepted")]
    Doctype,
    #[error("the XML document is missing {0}")]
    Missing(&'static str),
    #[error("the XML document has a bad number in {0}")]
    BadNumber(&'static str),
    #[error("the XML document has a bad date")]
    BadDate,
    #[error("the XML document has a bad key")]
    BadKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub text: String,
    pub children: Vec<Element>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPage {
    pub objects: Vec<ObjectEntry>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectEntry {
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub modified_s: i64,
    pub class: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionEntry {
    pub key: String,
    pub version: String,
    pub latest: bool,
    pub marker: bool,
    pub size: u64,
    pub modified_s: i64,
}

struct Frame {
    qualified_name: String,
    element: Element,
}

/// Reads the small, element-and-text subset of XML used by S3 responses.
pub fn parse(text: &str) -> Result<Element, XmlError> {
    if text.len() > MAX_BYTES {
        return Err(XmlError::TooLarge);
    }

    let mut position = 0;
    let mut frames: Vec<Frame> = Vec::new();
    let mut root = None;
    let mut elements = 0_usize;
    let mut declaration_seen = false;

    while position < text.len() {
        let remaining = text.get(position..).ok_or(XmlError::Malformed)?;
        if !remaining.starts_with('<') {
            let relative_end = remaining.find('<').unwrap_or(remaining.len());
            let end = position
                .checked_add(relative_end)
                .ok_or(XmlError::TooLarge)?;
            let piece = text.get(position..end).ok_or(XmlError::Malformed)?;
            if let Some(frame) = frames.last_mut() {
                frame.element.text.push_str(&decode_entities(piece)?);
            } else if !piece.trim().is_empty() {
                return Err(XmlError::Malformed);
            }
            position = end;
            continue;
        }

        if remaining.starts_with("<!DOCTYPE") {
            return Err(XmlError::Doctype);
        }
        if remaining.starts_with("<!--") {
            let end = remaining.find("-->").ok_or(XmlError::Malformed)?;
            position = position
                .checked_add(end)
                .and_then(|value| value.checked_add(3))
                .ok_or(XmlError::TooLarge)?;
            continue;
        }
        if remaining.starts_with("<![CDATA[") {
            let frame = frames.last_mut().ok_or(XmlError::Malformed)?;
            let body = remaining.get(9..).ok_or(XmlError::Malformed)?;
            let end = body.find("]]>").ok_or(XmlError::Malformed)?;
            frame
                .element
                .text
                .push_str(body.get(..end).ok_or(XmlError::Malformed)?);
            position = position
                .checked_add(9)
                .and_then(|value| value.checked_add(end))
                .and_then(|value| value.checked_add(3))
                .ok_or(XmlError::TooLarge)?;
            continue;
        }
        if remaining.starts_with("<?") {
            if declaration_seen
                || root.is_some()
                || !frames.is_empty()
                || !remaining.starts_with("<?xml")
            {
                return Err(XmlError::Malformed);
            }
            let end = remaining.find("?>").ok_or(XmlError::Malformed)?;
            let declaration = remaining.get(..end).ok_or(XmlError::Malformed)?;
            if !declaration
                .get(5..)
                .is_some_and(|tail| tail.is_empty() || tail.starts_with(char::is_whitespace))
            {
                return Err(XmlError::Malformed);
            }
            declaration_seen = true;
            position = position
                .checked_add(end)
                .and_then(|value| value.checked_add(2))
                .ok_or(XmlError::TooLarge)?;
            continue;
        }
        if remaining.starts_with("</") {
            let end = remaining.find('>').ok_or(XmlError::Malformed)?;
            let qualified_name = remaining
                .get(2..end)
                .map(str::trim)
                .filter(|name| valid_name(name))
                .ok_or(XmlError::Malformed)?;
            let frame = frames.pop().ok_or(XmlError::Malformed)?;
            if frame.qualified_name != qualified_name {
                return Err(XmlError::Malformed);
            }
            attach(frame.element, &mut frames, &mut root)?;
            position = position
                .checked_add(end)
                .and_then(|value| value.checked_add(1))
                .ok_or(XmlError::TooLarge)?;
            continue;
        }
        if remaining.starts_with("<!") {
            return Err(XmlError::Malformed);
        }

        let end = tag_end(remaining)?;
        let inside = remaining.get(1..end).ok_or(XmlError::Malformed)?;
        let trimmed = inside.trim_end();
        let (body, self_closing) = match trimmed.strip_suffix('/') {
            Some(body) => (body.trim_end(), true),
            None => (trimmed, false),
        };
        let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
        let qualified_name = body
            .get(..name_end)
            .filter(|name| valid_name(name))
            .ok_or(XmlError::Malformed)?;
        if body
            .get(name_end..)
            .is_some_and(|tail| !tail.is_empty() && !tail.starts_with(char::is_whitespace))
        {
            return Err(XmlError::Malformed);
        }
        elements = elements.checked_add(1).ok_or(XmlError::TooLarge)?;
        if elements > MAX_ELEMENTS {
            return Err(XmlError::TooLarge);
        }
        if frames.len().checked_add(1).ok_or(XmlError::TooDeep)? > MAX_DEPTH {
            return Err(XmlError::TooDeep);
        }
        let local_name = qualified_name
            .rsplit_once(':')
            .map_or(qualified_name, |(_, local)| local);
        let element = Element {
            name: local_name.to_owned(),
            text: String::new(),
            children: Vec::new(),
        };
        if self_closing {
            attach(element, &mut frames, &mut root)?;
        } else {
            frames.push(Frame {
                qualified_name: qualified_name.to_owned(),
                element,
            });
        }
        position = position
            .checked_add(end)
            .and_then(|value| value.checked_add(1))
            .ok_or(XmlError::TooLarge)?;
    }

    if !frames.is_empty() {
        return Err(XmlError::Malformed);
    }
    root.ok_or(XmlError::Malformed)
}

fn tag_end(text: &str) -> Result<usize, XmlError> {
    let mut quote = None;
    for (offset, character) in text.char_indices().skip(1) {
        match (quote, character) {
            (Some(open), close) if open == close => quote = None,
            (None, open @ ('\'' | '"')) => quote = Some(open),
            (None, '>') => return Ok(offset),
            _ => {}
        }
    }
    Err(XmlError::Malformed)
}

fn valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || matches!(first, '_' | ':'))
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | ':' | '-' | '.')
        })
        && !name.ends_with(':')
}

fn attach(
    element: Element,
    frames: &mut [Frame],
    root: &mut Option<Element>,
) -> Result<(), XmlError> {
    if let Some(parent) = frames.last_mut() {
        parent.element.children.push(element);
        return Ok(());
    }
    if root.is_some() {
        return Err(XmlError::Malformed);
    }
    *root = Some(element);
    Ok(())
}

fn decode_entities(text: &str) -> Result<String, XmlError> {
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        decoded.push_str(rest.get(..start).ok_or(XmlError::Malformed)?);
        let entity_and_after = rest.get(start + 1..).ok_or(XmlError::Malformed)?;
        let end = entity_and_after.find(';').ok_or(XmlError::Malformed)?;
        let entity = entity_and_after.get(..end).ok_or(XmlError::Malformed)?;
        match entity {
            "amp" => decoded.push('&'),
            "lt" => decoded.push('<'),
            "gt" => decoded.push('>'),
            "quot" => decoded.push('"'),
            "apos" => decoded.push('\''),
            numeric if numeric.starts_with("#x") => {
                decoded.push(numeric_entity(numeric.get(2..), 16)?);
            }
            numeric if numeric.starts_with('#') => {
                decoded.push(numeric_entity(numeric.get(1..), 10)?);
            }
            _ => return Err(XmlError::Malformed),
        }
        rest = entity_and_after.get(end + 1..).ok_or(XmlError::Malformed)?;
    }
    decoded.push_str(rest);
    Ok(decoded)
}

fn numeric_entity(digits: Option<&str>, radix: u32) -> Result<char, XmlError> {
    let digits = digits
        .filter(|digits| !digits.is_empty())
        .ok_or(XmlError::Malformed)?;
    let value = u32::from_str_radix(digits, radix).map_err(|_| XmlError::Malformed)?;
    char::from_u32(value).ok_or(XmlError::Malformed)
}

fn child<'a>(element: &'a Element, name: &str) -> Option<&'a Element> {
    element.children.iter().find(|child| child.name == name)
}

fn required<'a>(element: &'a Element, name: &'static str) -> Result<&'a str, XmlError> {
    child(element, name)
        .map(|child| child.text.as_str())
        .ok_or(XmlError::Missing(name))
}

fn root_named(text: &str, name: &str) -> Result<Element, XmlError> {
    let root = parse(text)?;
    if root.name == name {
        Ok(root)
    } else {
        Err(XmlError::Malformed)
    }
}

fn boolean(element: &Element, name: &'static str) -> Result<bool, XmlError> {
    match required(element, name)?.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(XmlError::Malformed),
    }
}

/// Reads one `ListObjectsV2` answer.
pub fn list_page(text: &str) -> Result<ListPage, XmlError> {
    let root = root_named(text, "ListBucketResult")?;
    let encoded = child(&root, "EncodingType").is_some_and(|value| value.text.trim() == "url");
    let mut objects = Vec::new();
    for contents in root
        .children
        .iter()
        .filter(|child| child.name == "Contents")
    {
        let raw_key = required(contents, "Key")?;
        let key = if encoded {
            percent_decode(raw_key)?
        } else {
            raw_key.to_owned()
        };
        let size = required(contents, "Size")?
            .trim()
            .parse::<u64>()
            .map_err(|_| XmlError::BadNumber("Size"))?;
        let modified_s =
            iso_date(required(contents, "LastModified")?.trim()).ok_or(XmlError::BadDate)?;
        objects.push(ObjectEntry {
            key,
            size,
            etag: required(contents, "ETag")?.to_owned(),
            modified_s,
            class: child(contents, "StorageClass").map(|value| value.text.trim().to_owned()),
        });
    }

    let truncated = boolean(&root, "IsTruncated")?;
    let next = if truncated {
        let token = required(&root, "NextContinuationToken")?;
        if token.is_empty() {
            return Err(XmlError::Missing("NextContinuationToken"));
        }
        Some(token.to_owned())
    } else {
        None
    };
    Ok(ListPage { objects, next })
}

/// Returns only the provider's machine-readable error code, never its message.
pub fn error_code(text: &str) -> Option<String> {
    let root = root_named(text, "Error").ok()?;
    child(&root, "Code").map(|code| code.text.clone())
}

/// Reads a bucket's versioning status. An absent status means versioning was never enabled.
pub fn versioning(text: &str) -> Result<Option<String>, XmlError> {
    let root = root_named(text, "VersioningConfiguration")?;
    let Some(status) = child(&root, "Status") else {
        return Ok(None);
    };
    match status.text.trim() {
        "Enabled" | "Suspended" => Ok(Some(status.text.trim().to_owned())),
        _ => Err(XmlError::Malformed),
    }
}

/// Reads versions and delete markers in the order returned by S3.
#[allow(clippy::type_complexity)] // The shared S3 XML interface fixes this return shape.
pub fn versions_page(
    text: &str,
) -> Result<(Vec<VersionEntry>, Option<(String, String)>), XmlError> {
    let root = root_named(text, "ListVersionsResult")?;
    let encoded = child(&root, "EncodingType").is_some_and(|value| value.text.trim() == "url");
    let mut entries = Vec::new();
    for item in root
        .children
        .iter()
        .filter(|child| matches!(child.name.as_str(), "Version" | "DeleteMarker"))
    {
        let marker = item.name == "DeleteMarker";
        let raw_key = required(item, "Key")?;
        let key = if encoded {
            percent_decode(raw_key)?
        } else {
            raw_key.to_owned()
        };
        let size = if marker {
            0
        } else {
            required(item, "Size")?
                .trim()
                .parse::<u64>()
                .map_err(|_| XmlError::BadNumber("Size"))?
        };
        entries.push(VersionEntry {
            key,
            version: required(item, "VersionId")?.to_owned(),
            latest: boolean(item, "IsLatest")?,
            marker,
            size,
            modified_s: iso_date(required(item, "LastModified")?.trim())
                .ok_or(XmlError::BadDate)?,
        });
    }

    let truncated = boolean(&root, "IsTruncated")?;
    let next = if truncated {
        let key = required(&root, "NextKeyMarker")?;
        let version = required(&root, "NextVersionIdMarker")?;
        if key.is_empty() {
            return Err(XmlError::Missing("NextKeyMarker"));
        }
        if version.is_empty() {
            return Err(XmlError::Missing("NextVersionIdMarker"));
        }
        Some((key.to_owned(), version.to_owned()))
    } else {
        None
    };
    Ok((entries, next))
}

/// Decodes S3's `encoding-type=url` key representation as UTF-8.
pub fn percent_decode(text: &str) -> Result<String, XmlError> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character == '%' {
            let high = characters
                .next()
                .and_then(hex_value)
                .ok_or(XmlError::BadKey)?;
            let low = characters
                .next()
                .and_then(hex_value)
                .ok_or(XmlError::BadKey)?;
            bytes.push((high << 4) | low);
        } else {
            let mut encoded = [0_u8; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        }
    }
    String::from_utf8(bytes).map_err(|_| XmlError::BadKey)
}

fn hex_value(character: char) -> Option<u8> {
    character
        .to_digit(16)
        .and_then(|value| u8::try_from(value).ok())
}

/// Escapes text for the small XML request bodies written by the S3 client.
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Reads an RFC 2822-style S3 header date, accepting only the `GMT` zone.
pub fn http_date(text: &str) -> Option<i64> {
    if !text.ends_with(" GMT") {
        return None;
    }
    OffsetDateTime::parse(text, &Rfc2822)
        .ok()
        .map(|date| date.unix_timestamp())
}

/// Reads an ISO 8601 S3 document date, accepting only the `Z` zone.
pub fn iso_date(text: &str) -> Option<i64> {
    if !text.ends_with('Z') {
        return None;
    }
    OffsetDateTime::parse(text, &Rfc3339)
        .ok()
        .map(|date| date.unix_timestamp())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    const TWO_OBJECTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>photos/puppy.jpg</Key>
    <LastModified>2009-10-12T17:50:30.000Z</LastModified>
    <ETag>&quot;abc&quot;</ETag>
    <Size>123</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <CommonPrefixes><Prefix>photos/</Prefix></CommonPrefixes>
  <Contents>
    <Key>notes.txt</Key>
    <LastModified>2009-10-12T17:51:30.999Z</LastModified>
    <ETag>&quot;def-2&quot;</ETag>
    <Size>0</Size>
  </Contents>
</ListBucketResult>"#;

    #[test]
    fn reads_a_two_object_listing_with_a_namespace_and_declaration() {
        let page = list_page(TWO_OBJECTS).unwrap();
        assert_eq!(page.next, None);
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].key, "photos/puppy.jpg");
        assert_eq!(page.objects[0].size, 123);
        assert_eq!(page.objects[0].etag, "\"abc\"");
        assert_eq!(page.objects[0].modified_s, 1_255_369_830);
        assert_eq!(page.objects[0].class.as_deref(), Some("STANDARD"));
        assert_eq!(page.objects[1].key, "notes.txt");
        assert_eq!(page.objects[1].modified_s, 1_255_369_890);
        assert_eq!(page.objects[1].class, None);
    }

    #[test]
    fn a_truncated_page_gives_the_next_token_and_a_final_page_gives_none() {
        let first = list_page(
            "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>next&amp;page</NextContinuationToken></ListBucketResult>",
        )
        .unwrap();
        assert_eq!(first.next.as_deref(), Some("next&page"));

        let final_page = list_page(
            "<ListBucketResult><IsTruncated>false</IsTruncated><NextContinuationToken>ignored</NextContinuationToken></ListBucketResult>",
        )
        .unwrap();
        assert_eq!(final_page.next, None);
    }

    #[test]
    fn truncated_without_a_token_is_an_error_not_an_end() {
        for xml in [
            "<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>",
            "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken/></ListBucketResult>",
        ] {
            assert_eq!(
                list_page(xml),
                Err(XmlError::Missing("NextContinuationToken"))
            );
        }
    }

    #[test]
    fn url_encoded_keys_are_decoded_and_bad_utf8_is_refused() {
        assert_eq!(percent_decode("a%20b").unwrap(), "a b");
        assert_eq!(percent_decode("%E2%82%AC").unwrap(), "€");
        assert_eq!(percent_decode("%FF"), Err(XmlError::BadKey));

        let xml = "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated><Contents><Key>a%20b</Key><LastModified>2009-10-12T17:50:30.000Z</LastModified><ETag>x</ETag><Size>1</Size></Contents></ListBucketResult>";
        assert_eq!(list_page(xml).unwrap().objects[0].key, "a b");
    }

    #[test]
    fn entities_and_character_references_are_decoded() {
        let root = parse(
            "<r>&amp;&lt;&gt;&quot;&apos;&#65;&#x20AC;<![CDATA[<&raw>]]><!-- ignored --></r>",
        )
        .unwrap();
        assert_eq!(root.text, "&<>\"'A€<&raw>");
        assert_eq!(parse("<r>&amp;lt;</r>").unwrap().text, "&lt;");
    }

    #[test]
    fn a_doctype_or_unknown_entity_is_refused() {
        let billion_laughs = "<!DOCTYPE lolz [<!ENTITY lol \"lol\">]><r>&lol;</r>";
        assert_eq!(parse(billion_laughs), Err(XmlError::Doctype));
        assert_eq!(parse("<r>&unknown;</r>"), Err(XmlError::Malformed));
    }

    #[test]
    fn too_deep_too_many_and_too_large_are_refused() {
        let too_deep = format!("{}x{}", "<a>".repeat(17), "</a>".repeat(17));
        assert_eq!(parse(&too_deep), Err(XmlError::TooDeep));

        let too_many = format!("<r>{}</r>", "<a/>".repeat(20_000));
        assert_eq!(parse(&too_many), Err(XmlError::TooLarge));

        let too_large = format!("<r>{}</r>", "x".repeat(8 * 1024 * 1024));
        assert_eq!(parse(&too_large), Err(XmlError::TooLarge));
    }

    #[test]
    fn every_truncation_of_a_valid_page_is_an_error_or_a_page_never_a_panic() {
        for end in 0..=TWO_OBJECTS.len() {
            if let Some(prefix) = TWO_OBJECTS.get(..end) {
                let answer = list_page(prefix);
                assert!(answer.is_err() || end == TWO_OBJECTS.len());
            }
        }
    }

    #[test]
    fn error_code_is_read_and_the_message_is_not_returned() {
        let xml = "<Error><Code>AccessDenied</Code><Message>secret provider text</Message></Error>";
        assert_eq!(error_code(xml).as_deref(), Some("AccessDenied"));
        assert_ne!(error_code(xml).as_deref(), Some("secret provider text"));
        assert_eq!(error_code("<broken>"), None);
    }

    #[test]
    fn versioning_is_enabled_suspended_or_absent() {
        assert_eq!(
            versioning(
                "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"
            )
            .unwrap(),
            Some("Enabled".to_owned())
        );
        assert_eq!(
            versioning(
                "<VersioningConfiguration><Status>Suspended</Status></VersioningConfiguration>"
            )
            .unwrap(),
            Some("Suspended".to_owned())
        );
        assert_eq!(versioning("<VersioningConfiguration/>").unwrap(), None);
    }

    #[test]
    fn versions_and_delete_markers_are_read_in_order() {
        let xml = r#"<ListVersionsResult>
          <IsTruncated>true</IsTruncated><NextKeyMarker>next-key</NextKeyMarker><NextVersionIdMarker>next-version</NextVersionIdMarker>
          <Version><Key>a.txt</Key><VersionId>v2</VersionId><IsLatest>true</IsLatest><LastModified>2009-10-12T17:50:30.123Z</LastModified><Size>9</Size></Version>
          <DeleteMarker><Key>a.txt</Key><VersionId>v1</VersionId><IsLatest>false</IsLatest><LastModified>2009-10-11T17:50:30.000Z</LastModified></DeleteMarker>
        </ListVersionsResult>"#;
        let (entries, next) = versions_page(xml).unwrap();
        assert_eq!(
            next,
            Some(("next-key".to_owned(), "next-version".to_owned()))
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "a.txt");
        assert_eq!(entries[0].version, "v2");
        assert!(entries[0].latest);
        assert!(!entries[0].marker);
        assert_eq!(entries[0].size, 9);
        assert!(entries[1].marker);
        assert!(!entries[1].latest);
        assert_eq!(entries[1].size, 0);
    }

    #[test]
    fn times_are_cut_to_seconds_and_both_date_forms_agree() {
        let iso = iso_date("2009-10-12T17:50:30.123Z").unwrap();
        let http = http_date("Mon, 12 Oct 2009 17:50:30 GMT").unwrap();
        assert_eq!(iso, http);
        assert_eq!(iso, 1_255_369_830);
        assert_eq!(iso_date("2009-10-12T17:50:30+01:00"), None);
        assert_eq!(http_date("Mon, 12 Oct 2009 17:50:30 +0100"), None);
    }

    #[test]
    fn escape_round_trips_through_parse() {
        let original = "&<>\"' text";
        let xml = format!("<r>{}</r>", escape(original));
        assert_eq!(parse(&xml).unwrap().text, original);
    }

    #[test]
    fn etags_keep_their_quotes_and_multipart_suffix() {
        let xml = "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>x</Key><LastModified>2009-10-12T17:50:30Z</LastModified><ETag>&quot;d41d8…-3&quot;</ETag><Size>1</Size></Contents></ListBucketResult>";
        assert_eq!(list_page(xml).unwrap().objects[0].etag, "\"d41d8…-3\"");
    }
}
