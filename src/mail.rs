use crate::{
    error::{ErrorCode as E, MailError, Result},
    model::*,
    wire::{Wire, decode_mailbox, encode_mailbox, raw, string},
};
use imap_proto::{
    AttributeValue as A, BodyStructure as B, MailboxDatum as M, NameAttribute as N, Response as R,
    ResponseCode as C, StatusAttribute as S,
};

#[derive(Clone)]
pub(crate) struct RawFolder {
    pub name: String,
    pub delimiter: Option<String>,
    pub selectable: bool,
    pub roles: Vec<String>,
}
pub(crate) struct Metadata {
    pub headers: Headers,
    pub flags: Vec<String>,
    pub received: String,
    pub size: u32,
    pub truncated: Vec<String>,
    pub structure: Option<B<'static>>,
}
#[derive(Clone)]
pub(crate) struct Part {
    pub section: String,
    pub html: bool,
    pub charset: String,
    pub encoding: String,
    pub size: usize,
}

impl Wire {
    pub async fn folders(&mut self) -> Result<Vec<RawFolder>> {
        let replies = self
            .command(
                vec![raw("LIST"), string(""), string("*")],
                E::UpstreamUnavailable,
            )
            .await?;
        let mut folders = Vec::new();
        for reply in replies {
            if let R::MailboxData(M::List(item)) = reply {
                let mut selectable = true;
                let mut roles = Vec::new();
                for attr in item.name_attributes {
                    let role = match attr {
                        N::NoSelect => {
                            selectable = false;
                            None
                        }
                        N::All => Some("all"),
                        N::Archive => Some("archive"),
                        N::Drafts => Some("drafts"),
                        N::Flagged => Some("flagged"),
                        N::Junk => Some("junk"),
                        N::Sent => Some("sent"),
                        N::Trash => Some("trash"),
                        _ => None,
                    };
                    if let Some(role) = role {
                        roles.push(role.into());
                    }
                }
                folders.push(RawFolder {
                    name: decode_mailbox(&item.name)?,
                    delimiter: item.delimiter.map(|d| d.into_owned()),
                    selectable,
                    roles,
                });
            }
        }
        if folders.len() > SNAPSHOT_LIMIT {
            return Err(MailError(E::SizeLimit));
        }
        folders.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        Ok(folders)
    }
    pub async fn counts(&mut self, folder: &str) -> Result<(Option<u32>, Option<u32>)> {
        let replies = match self
            .command(
                vec![
                    raw("STATUS"),
                    string(encode_mailbox(folder)),
                    raw("(MESSAGES UNSEEN)"),
                ],
                E::MailboxNotFound,
            )
            .await
        {
            Ok(replies) => replies,
            Err(MailError(E::MailboxNotFound)) => return Ok((None, None)),
            Err(error) => return Err(error),
        };
        let (mut messages, mut unread) = (None, None);
        for r in replies {
            if let R::MailboxData(M::Status { status, .. }) = r {
                for s in status {
                    match s {
                        S::Messages(n) => messages = Some(n),
                        S::Unseen(n) => unread = Some(n),
                        _ => {}
                    }
                }
            }
        }
        Ok((messages, unread))
    }
    pub async fn examine(&mut self, folder: &str) -> Result<(u32, u32)> {
        let replies = self
            .command(
                vec![raw("EXAMINE"), string(encode_mailbox(folder))],
                E::MailboxNotFound,
            )
            .await?;
        let (mut validity, mut next, mut read_only) = (None, None, false);
        for r in replies {
            let tagged = matches!(&r, R::Done { .. });
            if let R::Data { outcome, .. } | R::Done { outcome, .. } = r {
                match outcome.code {
                    Some(C::UidValidity(n)) => validity = Some(n),
                    Some(C::UidNext(n)) => next = Some(n),
                    Some(C::ReadOnly) if tagged => read_only = true,
                    Some(C::ReadWrite) if tagged && self.allow_icloud_examine_read_write => {
                        read_only = true;
                    }
                    Some(C::ReadWrite) => {
                        tracing::warn!(target: "icloud_mail_mcp::ops", phase = "read_write_selection", "EXAMINE was not read-only");
                        return Err(MailError(E::UpstreamUnavailable));
                    }
                    _ => {}
                }
            }
        }
        match (validity, next, read_only) {
            (Some(v), Some(n), true) if v > 0 && n > 0 => Ok((v, n)),
            _ => {
                tracing::warn!(target: "icloud_mail_mcp::ops", has_validity = validity.is_some(), has_uidnext = next.is_some(), read_only, "EXAMINE identity incomplete");
                Err(MailError(E::UpstreamUnavailable))
            }
        }
    }
    pub async fn search(&mut self, high: u32, filters: &Filters) -> Result<Vec<u32>> {
        if high == 0 {
            return Ok(Vec::new());
        }
        let unicode = [
            &filters.from_contains,
            &filters.to_contains,
            &filters.subject_contains,
            &filters.text_contains,
        ]
        .into_iter()
        .flatten()
        .any(|s| !s.is_ascii());
        let mut args = vec![
            raw(if unicode {
                "UID SEARCH CHARSET UTF-8"
            } else {
                "UID SEARCH"
            }),
            raw(format!("UID 1:{high}")),
        ];
        for (key, value) in [
            ("FROM", &filters.from_contains),
            ("TO", &filters.to_contains),
            ("SUBJECT", &filters.subject_contains),
            ("TEXT", &filters.text_contains),
        ] {
            if let Some(value) = value {
                args.push(raw(key));
                args.push(string(value));
            }
        }
        if let Some(unread) = filters.unread {
            args.push(raw(if unread { "UNSEEN" } else { "SEEN" }));
        }
        if let Some(flagged) = filters.flagged {
            args.push(raw(if flagged { "FLAGGED" } else { "UNFLAGGED" }));
        }
        for (key, date) in [
            ("SINCE", &filters.received_on_or_after),
            ("BEFORE", &filters.received_before),
        ] {
            if let Some(date) = date {
                let date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
                    .map_err(|_| MailError(E::InternalError))?;
                args.push(raw(key));
                args.push(raw(date.format("%d-%b-%Y").to_string()));
            }
        }
        let replies = self.command(args, E::UnsupportedSearch).await?;
        let mut ids = Vec::new();
        for r in replies {
            if let R::MailboxData(M::Search(found)) = r {
                ids.extend(found);
            }
        }
        if ids.len() > SNAPSHOT_LIMIT || ids.iter().any(|id| *id == 0 || *id > high) {
            return Err(MailError(E::SizeLimit));
        }
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids.dedup();
        Ok(ids)
    }
    pub async fn metadata(&mut self, uid: u32, full: bool) -> Result<Option<Metadata>> {
        let fields = if full {
            "UID FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (FROM TO CC SUBJECT DATE)]"
        } else {
            "UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER.FIELDS (FROM TO CC SUBJECT DATE)]"
        };
        let replies = self
            .command(
                vec![raw(format!("UID FETCH {uid} ({fields})"))],
                E::UpstreamUnavailable,
            )
            .await?;
        let mut found = None;
        for r in replies {
            if let R::Fetch(_, attrs) = r {
                if !attrs.iter().any(|a| matches!(a,A::Uid(n) if *n==uid)) {
                    continue;
                }
                if !attrs.iter().any(|a| matches!(a, A::Rfc822Size(_))) {
                    continue;
                }
                let (mut flags, mut received, mut size, mut header, mut structure) =
                    (Vec::new(), None, None, Vec::new(), None);
                for a in attrs {
                    match a {
                        A::Flags(f) => flags = f.into_iter().map(|v| v.into_owned()).collect(),
                        A::InternalDate(d) => {
                            received = Some(
                                chrono::DateTime::parse_from_str(&d, "%d-%b-%Y %H:%M:%S %z")
                                    .map_err(|_| {
                                        tracing::warn!(target: "icloud_mail_mcp::ops", phase = "internal_date", "Message date rejected");
                                        MailError(E::UpstreamUnavailable)
                                    })?
                                    .to_rfc3339(),
                            )
                        }
                        A::Rfc822Size(n) => size = Some(n),
                        A::BodySection {
                            data: Some(bytes), ..
                        } => header = bytes.into_owned(),
                        A::BodyStructure(b) => structure = Some(b),
                        _ => {}
                    }
                }
                if header.len() > 65536 {
                    return Err(MailError(E::SizeLimit));
                }
                let (headers, truncated) = parse_headers(&header)?;
                found = Some(Metadata {
                    headers,
                    flags,
                    received: received.ok_or_else(|| {
                        tracing::warn!(target: "icloud_mail_mcp::ops", phase = "missing_internal_date", "Message metadata incomplete");
                        MailError(E::UpstreamUnavailable)
                    })?,
                    size: size.ok_or(MailError(E::UpstreamUnavailable))?,
                    truncated,
                    structure,
                });
            }
        }
        Ok(found)
    }
    pub async fn body_bytes(&mut self, uid: u32, part: &Part) -> Result<Vec<u8>> {
        if part.size > TEXT_LIMIT {
            return Err(MailError(E::SizeLimit));
        }
        let mut bytes = Vec::with_capacity(part.size);
        while bytes.len() < part.size {
            let offset = bytes.len();
            let count = (part.size - offset).min(65536);
            let args = format!(
                "UID FETCH {uid} (UID BODY.PEEK[{}]<{offset}.{count}>)",
                part.section
            );
            let replies = self
                .command(vec![raw(args)], E::UpstreamUnavailable)
                .await?;
            let mut data = None;
            for r in replies {
                if let R::Fetch(_, attrs) = r {
                    if !attrs.iter().any(|a| matches!(a,A::Uid(n) if *n==uid)) {
                        continue;
                    }
                    for a in attrs {
                        if let A::BodySection {
                            section,
                            index,
                            data: Some(b),
                        } = a
                        {
                            let expected = if part.section == "TEXT" {
                                imap_proto::SectionPath::Full(imap_proto::MessageSection::Text)
                            } else {
                                let numbers: std::result::Result<Vec<u32>, _> =
                                    part.section.split('.').map(str::parse).collect();
                                imap_proto::SectionPath::Part(
                                    numbers.map_err(|_| MailError(E::InternalError))?,
                                    None,
                                )
                            };
                            if section.as_ref() != Some(&expected)
                                || index != Some(offset as u32)
                                || b.len() > count
                            {
                                return Err(MailError(E::UpstreamUnavailable));
                            }
                            data = Some(b.into_owned());
                        }
                    }
                }
            }
            let data = data.ok_or(MailError(E::MessageNotFound))?;
            if data.is_empty() || data.len() < count {
                return Err(MailError(E::UpstreamUnavailable));
            }
            bytes.extend_from_slice(&data);
        }
        Ok(bytes)
    }
}

fn parser() -> mail_parser::MessageParser {
    mail_parser::MessageParser::new()
        .max_depth(32)
        .max_parts(256)
        .max_encoded_nesting(0)
}
fn parse_headers(bytes: &[u8]) -> Result<(Headers, Vec<String>)> {
    let mut input = Vec::from(bytes);
    if !input.ends_with(b"\r\n\r\n") {
        input.extend_from_slice(b"\r\n\r\n");
    }
    let message = parser()
        .parse_headers(&input)
        .ok_or_else(|| {
            tracing::warn!(target: "icloud_mail_mcp::ops", phase = "message_headers", empty = bytes.is_empty(), "Message headers rejected");
            MailError(E::UpstreamUnavailable)
        })?;
    let mut truncated = Vec::new();
    let mut remaining = 50;
    let mut addresses = |list: Option<mail_parser::AddressList<'_>>, field: &str| {
        let mut out = Vec::new();
        if let Some(list) = list {
            for a in list.mailboxes() {
                if remaining == 0 {
                    truncated.push(field.into());
                    break;
                }
                remaining -= 1;
                let mut cap = |s: &str| {
                    if s.len() > 2048 {
                        truncated.push(field.into());
                    }
                    limited(s, 2048)
                };
                out.push(Address {
                    name: a.name().map(&mut cap),
                    address: a.address().map(&mut cap),
                });
            }
        }
        out
    };
    let from = addresses(message.from(), "from");
    let to = addresses(message.to(), "to");
    let cc = addresses(message.cc(), "cc");
    let subject = message.subject().map(|s| {
        if s.len() > 2048 {
            truncated.push("subject".into());
        }
        limited(s, 2048)
    });
    let mut headers = Headers {
        from,
        to,
        cc,
        subject,
        header_date: message.date().map(|d| d.to_rfc3339()),
    };
    while serde_json::to_vec(&headers)
        .map_err(|_| MailError(E::InternalError))?
        .len()
        > 24 * 1024
    {
        if headers.cc.pop().is_some() {
            truncated.push("cc".into());
        } else if headers.to.pop().is_some() {
            truncated.push("to".into());
        } else if headers.from.pop().is_some() {
            truncated.push("from".into());
        } else {
            return Err(MailError(E::SizeLimit));
        }
    }
    truncated.sort();
    truncated.dedup();
    Ok((headers, truncated))
}
fn param(params: &imap_proto::BodyParams<'_>, name: &str) -> Option<String> {
    params
        .as_ref()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.to_string())
}
fn common<'b, 'a>(b: &'b B<'a>) -> &'b imap_proto::BodyContentCommon<'a> {
    match b {
        B::Basic { common, .. }
        | B::Text { common, .. }
        | B::Message { common, .. }
        | B::Multipart { common, .. } => common,
    }
}
pub(crate) fn select_parts(body: &B<'_>) -> Result<(Option<Part>, Vec<Attachment>)> {
    fn validate(b: &B<'_>, depth: usize, count: &mut usize) -> Result<()> {
        *count += 1;
        if depth > 32 || *count > 256 {
            return Err(MailError(E::SizeLimit));
        }
        match b {
            B::Multipart { bodies, .. } => {
                for child in bodies {
                    validate(child, depth + 1, count)?;
                }
            }
            B::Message { body, .. } => validate(body, depth + 1, count)?,
            _ => {}
        }
        Ok(())
    }
    validate(body, 1, &mut 0)?;
    fn walk(
        b: &B<'_>,
        path: String,
        depth: usize,
        count: &mut usize,
        attachments: &mut Vec<Attachment>,
    ) -> Result<Option<Part>> {
        *count += 1;
        if depth > 32 || *count > 256 {
            return Err(MailError(E::SizeLimit));
        }
        let c = common(b);
        let filename = c
            .disposition
            .as_ref()
            .and_then(|d| param(&d.params, "filename"))
            .or_else(|| param(&c.ty.params, "name"));
        let attachment = filename.is_some()
            || c.disposition
                .as_ref()
                .is_some_and(|d| d.ty.eq_ignore_ascii_case("attachment"))
            || matches!(b, B::Message { .. });
        if attachment {
            let size = match b {
                B::Basic { other, .. } | B::Text { other, .. } | B::Message { other, .. } => {
                    Some(other.octets)
                }
                _ => None,
            };
            attachments.push(Attachment {
                part_ref: if path.is_empty() { "1".into() } else { path },
                filename: filename.map(|s| limited(&s, 2048)),
                media_type: Some(format!("{}/{}", c.ty.ty, c.ty.subtype)),
                size_bytes: size,
            });
            return Ok(None);
        }
        match b {
            B::Text { other, .. }
                if c.ty.subtype.eq_ignore_ascii_case("plain")
                    || c.ty.subtype.eq_ignore_ascii_case("html") =>
            {
                let encoding = match &other.transfer_encoding {
                    imap_proto::ContentEncoding::SevenBit => "7bit".into(),
                    imap_proto::ContentEncoding::EightBit => "8bit".into(),
                    imap_proto::ContentEncoding::Binary => "binary".into(),
                    imap_proto::ContentEncoding::Base64 => "base64".into(),
                    imap_proto::ContentEncoding::QuotedPrintable => "quoted-printable".into(),
                    _ => return Err(MailError(E::UpstreamUnavailable)),
                };
                Ok(Some(Part {
                    section: if path.is_empty() { "TEXT".into() } else { path },
                    html: c.ty.subtype.eq_ignore_ascii_case("html"),
                    charset: param(&c.ty.params, "charset").unwrap_or("us-ascii".into()),
                    encoding,
                    size: other.octets as usize,
                }))
            }
            B::Multipart { bodies, .. } => {
                let mut choices = Vec::new();
                for (i, child) in bodies.iter().enumerate() {
                    let p = if path.is_empty() {
                        format!("{}", i + 1)
                    } else {
                        format!("{path}.{}", i + 1)
                    };
                    choices.push(walk(child, p, depth + 1, count, attachments)?);
                }
                if c.ty.subtype.eq_ignore_ascii_case("alternative") {
                    Ok(choices
                        .iter()
                        .flatten()
                        .find(|p| !p.html)
                        .cloned()
                        .or_else(|| choices.into_iter().flatten().next()))
                } else if c.ty.subtype.eq_ignore_ascii_case("related") {
                    let index = param(&c.ty.params, "start")
                        .and_then(|start| {
                            bodies.iter().position(|b| match b {
                                B::Basic { other, .. }
                                | B::Text { other, .. }
                                | B::Message { other, .. } => {
                                    other.id.as_deref() == Some(start.as_str())
                                }
                                _ => false,
                            })
                        })
                        .unwrap_or(0);
                    Ok(choices.get(index).cloned().flatten())
                } else {
                    Ok(choices.into_iter().flatten().next())
                }
            }
            _ => Ok(None),
        }
    }
    let mut attachments = Vec::new();
    let mut count = 0;
    let part = walk(body, String::new(), 1, &mut count, &mut attachments)?;
    Ok((part, attachments))
}

pub(crate) fn decode(part: &Part, bytes: Vec<u8>) -> Result<(String, Vec<String>)> {
    use base64::Engine;
    if bytes.len() > TEXT_LIMIT {
        return Err(MailError(E::SizeLimit));
    }
    let bytes = match part.encoding.as_str() {
        "base64" => {
            let clean: Vec<_> = bytes
                .into_iter()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
            base64::engine::general_purpose::STANDARD
                .decode(clean)
                .map_err(|_| MailError(E::UnsupportedContent))?
        }
        "quoted-printable" => {
            let mut out = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'=' {
                    if bytes.get(i + 1..i + 3) == Some(b"\r\n") {
                        i += 3;
                        continue;
                    }
                    let hex = bytes
                        .get(i + 1..i + 3)
                        .filter(|s| s.iter().all(u8::is_ascii_hexdigit))
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .and_then(|s| u8::from_str_radix(s, 16).ok())
                        .ok_or(MailError(E::UnsupportedContent))?;
                    out.push(hex);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            out
        }
        "7bit" | "8bit" | "binary" => bytes,
        _ => return Err(MailError(E::UnsupportedContent)),
    };
    let encoding = encoding_rs::Encoding::for_label(part.charset.as_bytes())
        .ok_or(MailError(E::UnsupportedContent))?;
    let mut text = String::with_capacity(bytes.len().saturating_mul(4).clamp(4, TEXT_LIMIT));
    let (status, read, errors) = encoding
        .new_decoder()
        .decode_to_string(&bytes, &mut text, true);
    if matches!(status, encoding_rs::CoderResult::OutputFull) || read != bytes.len() {
        return Err(MailError(E::SizeLimit));
    }
    let warnings = if errors {
        vec!["decoding_replacement".into()]
    } else {
        Vec::new()
    };
    if part.html {
        text = html_text(&text)?;
    }
    Ok((text, warnings))
}

fn html_text(input: &str) -> Result<String> {
    let mut out = String::with_capacity(input.len().min(TEXT_LIMIT));
    let mut i = 0;
    let mut skip: Option<(String, usize)> = None;
    fn append(out: &mut String, text: &str) -> Result<()> {
        if out.len() + text.len() > TEXT_LIMIT {
            return Err(MailError(E::SizeLimit));
        }
        out.push_str(text);
        Ok(())
    }
    while i < input.len() {
        if input[i..].starts_with("<!--") {
            i = input[i + 4..]
                .find("-->")
                .map_or(input.len(), |n| i + 4 + n + 3);
            continue;
        }
        if input.as_bytes()[i] == b'<' {
            let mut end = i + 1;
            let mut quote = None;
            while end < input.len() {
                let b = input.as_bytes()[end];
                if let Some(q) = quote {
                    if b == q {
                        quote = None;
                    }
                } else if b == b'\'' || b == b'"' {
                    quote = Some(b);
                } else if b == b'>' {
                    break;
                }
                end += 1;
            }
            if end == input.len() {
                // No closing delimiter exists. Consume the remaining tail once,
                // rather than rescanning it for each following '<'.
                if skip.is_none() {
                    append(&mut out, &input[i..])?;
                }
                break;
            }
            if end < input.len() {
                let tag = input[i + 1..end].trim();
                let close = tag.starts_with('/');
                let name = tag
                    .trim_start_matches('/')
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if let Some((kind, depth)) = &mut skip {
                    if name == *kind {
                        if close {
                            *depth -= 1;
                        } else if !tag.ends_with('/') {
                            *depth += 1;
                        }
                    }
                    if *depth > 32 {
                        return Err(MailError(E::SizeLimit));
                    }
                    if *depth == 0 {
                        skip = None;
                    }
                } else if !close
                    && matches!(name.as_str(), "head" | "script" | "style" | "template")
                {
                    skip = Some((name, 1));
                } else if matches!(name.as_str(), "br" | "p" | "div" | "li" | "tr")
                    && !out.ends_with('\n')
                {
                    append(&mut out, "\n")?;
                }
                i = end + 1;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap();
        if skip.is_none() {
            if ch == '&'
                && let Some(end) = input[i..].chars().take(32).position(|c| c == ';')
            {
                let bytes_end = input[i..].char_indices().nth(end).unwrap().0 + 1;
                let mut decoded = String::new();
                mail_parser::add_html_token(
                    &mut decoded,
                    &input.as_bytes()[i..i + bytes_end],
                    false,
                );
                append(&mut out, &decoded)?;
                i += bytes_end;
                continue;
            }
            if ch.is_whitespace() {
                if !out.ends_with(char::is_whitespace) && !out.is_empty() {
                    append(&mut out, " ")?;
                }
            } else {
                let mut buf = [0; 4];
                append(&mut out, ch.encode_utf8(&mut buf))?;
            }
        }
        i += ch.len_utf8();
    }
    Ok(out)
}
