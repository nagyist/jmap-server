use std::{
    collections::{btree_map, hash_map::Entry, BTreeMap, HashMap},
    iter::FromIterator,
};

use jmap::{
    changes::JMAPChanges,
    id::{BlobId, JMAPIdSerialize},
    json::JSONValue,
    request::GetRequest,
    JMAPError,
};

use crate::mail::{
    parse::{
        header_to_jmap_address, header_to_jmap_date, header_to_jmap_id, header_to_jmap_text,
        header_to_jmap_url,
    },
    HeaderName, JMAPMailHeaders, Keyword, MailBodyProperties, MailHeaderForm, MailHeaderProperty,
    MailProperties, MessageData, MessageField, MessageOutline, MimePart, MimePartType,
    MESSAGE_DATA, MESSAGE_PARTS, MESSAGE_RAW,
};
use mail_parser::{
    parsers::{
        fields::{
            address::parse_address, date::parse_date, id::parse_id,
            unstructured::parse_unstructured,
        },
        message::MessageStream,
        preview::{preview_html, preview_text, truncate_html, truncate_text},
    },
    HeaderOffset, HeaderValue, MessageStructure, RfcHeader,
};
use store::{
    blob::BlobIndex, leb128::Leb128, roaring::RoaringBitmap, Collection, JMAPId, JMAPStore,
};
use store::{serialize::StoreDeserialize, JMAPIdPrefix};
use store::{DocumentId, Store, StoreError};

#[derive(Debug, Default)]
pub struct MailGetArguments {
    pub body_properties: Vec<MailBodyProperties>,
    pub fetch_text_body_values: bool,
    pub fetch_html_body_values: bool,
    pub fetch_all_body_values: bool,
    pub max_body_value_bytes: usize,
}

impl MailGetArguments {
    pub fn parse_arguments(arguments: HashMap<String, JSONValue>) -> jmap::Result<Self> {
        let mut body_properties = None;
        let mut fetch_text_body_values = false;
        let mut fetch_html_body_values = false;
        let mut fetch_all_body_values = false;
        let mut max_body_value_bytes = 0;

        for (arg_name, arg_value) in arguments {
            match arg_name.as_str() {
                "bodyProperties" => body_properties = arg_value.parse_array_items(true)?,
                "fetchTextBodyValues" => fetch_text_body_values = arg_value.parse_bool()?,
                "fetchHtmlBodyValues" => fetch_html_body_values = arg_value.parse_bool()?,
                "fetchAllBodyValues" => fetch_all_body_values = arg_value.parse_bool()?,
                "maxBodyValueBytes" => {
                    max_body_value_bytes = arg_value.parse_unsigned_int(false)?.unwrap() as usize
                }
                _ => {
                    return Err(JMAPError::InvalidArguments(format!(
                        "Unknown argument: '{}'.",
                        arg_name
                    )))
                }
            }
        }

        Ok(MailGetArguments {
            body_properties: body_properties.unwrap_or_else(|| {
                vec![
                    MailBodyProperties::PartId,
                    MailBodyProperties::BlobId,
                    MailBodyProperties::Size,
                    MailBodyProperties::Name,
                    MailBodyProperties::Type,
                    MailBodyProperties::Charset,
                    MailBodyProperties::Disposition,
                    MailBodyProperties::Cid,
                    MailBodyProperties::Language,
                    MailBodyProperties::Location,
                ]
            }),
            fetch_text_body_values,
            fetch_html_body_values,
            fetch_all_body_values,
            max_body_value_bytes,
        })
    }
}

trait BlobIdClone {
    fn clone_with_index(&self, blob_index: BlobIndex) -> Self;
}

impl BlobIdClone for BlobId {
    fn clone_with_index(&self, blob_index: BlobIndex) -> Self {
        match self {
            BlobId::Owned(owned) => {
                let mut owned = owned.clone();
                owned.blob_index = blob_index + MESSAGE_PARTS;
                BlobId::Owned(owned)
            }
            BlobId::InnerOwned(inner) => {
                let mut inner = inner.clone();
                inner.blob_index = blob_index;
                BlobId::InnerOwned(inner)
            }
            BlobId::InnerTemporary(inner) => {
                let mut inner = inner.clone();
                inner.blob_index = blob_index;
                BlobId::InnerTemporary(inner)
            }
            BlobId::Temporary(_) => unreachable!(),
        }
    }
}

pub trait JMAPMailGet {
    fn mail_get(&self, request: GetRequest) -> jmap::Result<JSONValue>;
}

impl<T> JMAPMailGet for JMAPStore<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn mail_get(&self, request: GetRequest) -> jmap::Result<JSONValue> {
        let properties = request
            .properties
            .parse_array_items(true)?
            .unwrap_or_else(|| {
                vec![
                    MailProperties::Id,
                    MailProperties::BlobId,
                    MailProperties::ThreadId,
                    MailProperties::MailboxIds,
                    MailProperties::Keywords,
                    MailProperties::Size,
                    MailProperties::ReceivedAt,
                    MailProperties::MessageId,
                    MailProperties::InReplyTo,
                    MailProperties::References,
                    MailProperties::Sender,
                    MailProperties::From,
                    MailProperties::To,
                    MailProperties::Cc,
                    MailProperties::Bcc,
                    MailProperties::ReplyTo,
                    MailProperties::Subject,
                    MailProperties::SentAt,
                    MailProperties::HasAttachment,
                    MailProperties::Preview,
                    MailProperties::BodyValues,
                    MailProperties::TextBody,
                    MailProperties::HtmlBody,
                    MailProperties::Attachments,
                ]
            });

        let arguments = MailGetArguments::parse_arguments(request.arguments)?;

        enum FetchRaw {
            Header,
            All,
            None,
        }

        let fetch_raw = if arguments.body_properties.iter().any(|prop| {
            matches!(
                prop,
                MailBodyProperties::Headers | MailBodyProperties::Header(_)
            )
        }) {
            FetchRaw::All
        } else if properties.iter().any(|prop| {
            matches!(
                prop,
                MailProperties::Header(MailHeaderProperty {
                    form: MailHeaderForm::Raw,
                    ..
                }) | MailProperties::Header(MailHeaderProperty {
                    header: HeaderName::Other(_),
                    ..
                }) | MailProperties::BodyStructure
            )
        }) {
            FetchRaw::Header
        } else {
            FetchRaw::None
        };

        let document_ids = self
            .get_document_ids(request.account_id, Collection::Mail)?
            .unwrap_or_else(RoaringBitmap::new);

        let request_ids: Vec<u64> = if let Some(request_ids) = request.ids {
            if request_ids.len() > self.config.max_objects_in_get {
                return Err(JMAPError::RequestTooLarge);
            } else {
                request_ids
            }
        } else if !document_ids.is_empty() {
            let document_ids = document_ids
                .iter()
                .take(self.config.max_objects_in_get)
                .collect::<Vec<DocumentId>>();
            self.get_multi_document_tag_id(
                request.account_id,
                Collection::Mail,
                document_ids.iter().copied(),
                MessageField::ThreadId.into(),
            )?
            .into_iter()
            .zip(document_ids)
            .filter_map(|(thread_id, document_id)| {
                JMAPId::from_parts(*thread_id?, document_id).into()
            })
            .collect::<Vec<u64>>()
        } else {
            Vec::new()
        };

        let mut not_found = Vec::new();
        let mut results = Vec::with_capacity(request_ids.len());

        for jmap_id in request_ids {
            let document_id = jmap_id.get_document_id();
            if !document_ids.contains(document_id) {
                not_found.push(jmap_id.to_jmap_string().into());
                continue;
            }

            let message_data_bytes = self
                .get_blob(
                    request.account_id,
                    Collection::Mail,
                    document_id,
                    MESSAGE_DATA,
                )?
                .ok_or(StoreError::DataCorruption)?;

            let (message_data_len, read_bytes) = usize::from_leb128_bytes(&message_data_bytes[..])
                .ok_or(StoreError::DataCorruption)?;

            let mut message_data = MessageData::deserialize(
                &message_data_bytes[read_bytes..read_bytes + message_data_len],
            )
            .ok_or(StoreError::DataCorruption)?;
            let (message_raw, mut message_outline) = match &fetch_raw {
                FetchRaw::All => (
                    Some(
                        self.get_blob(
                            request.account_id,
                            Collection::Mail,
                            document_id,
                            MESSAGE_RAW,
                        )?
                        .ok_or(StoreError::DataCorruption)?,
                    ),
                    Some(
                        MessageOutline::deserialize(
                            &message_data_bytes[read_bytes + message_data_len..],
                        )
                        .ok_or(StoreError::DataCorruption)?,
                    ),
                ),
                FetchRaw::Header => {
                    let message_outline = MessageOutline::deserialize(
                        &message_data_bytes[read_bytes + message_data_len..],
                    )
                    .ok_or(StoreError::DataCorruption)?;
                    (
                        Some(
                            self.get_blob_range(
                                request.account_id,
                                Collection::Mail,
                                document_id,
                                MESSAGE_RAW,
                                0..message_outline.body_offset as u32,
                            )?
                            .ok_or(StoreError::DataCorruption)?,
                        ),
                        Some(message_outline),
                    )
                }
                FetchRaw::None => (None, None),
            };

            let message_raw_ref = message_raw.as_ref().map(|raw| raw.as_ref());
            let base_blob_id =
                BlobId::new_owned(request.account_id, Collection::Mail, document_id, 0);
            let mut result: HashMap<String, JSONValue> = HashMap::new();

            for property in &properties {
                if let Entry::Vacant(entry) = result.entry(property.to_string()) {
                    let value = match property {
                        MailProperties::Header(MailHeaderProperty {
                            form: form @ MailHeaderForm::Raw,
                            header,
                            all,
                        })
                        | MailProperties::Header(MailHeaderProperty {
                            form,
                            header: header @ HeaderName::Other(_),
                            all,
                        }) => {
                            if let Some(offsets) = message_outline
                                .as_mut()
                                .unwrap()
                                .headers
                                .get_mut(0)
                                .and_then(|l| l.remove(header))
                            {
                                add_raw_header(
                                    &offsets,
                                    message_raw.as_ref().unwrap(),
                                    form.clone(),
                                    *all,
                                )
                            } else {
                                JSONValue::Null
                            }
                        }
                        MailProperties::MessageId => add_rfc_header(
                            &mut message_data.properties,
                            RfcHeader::MessageId,
                            MailHeaderForm::MessageIds,
                            false,
                        )?,
                        MailProperties::InReplyTo => add_rfc_header(
                            &mut message_data.properties,
                            RfcHeader::InReplyTo,
                            MailHeaderForm::MessageIds,
                            false,
                        )?,
                        MailProperties::References => add_rfc_header(
                            &mut message_data.properties,
                            RfcHeader::References,
                            MailHeaderForm::MessageIds,
                            false,
                        )?,
                        MailProperties::Sender => add_rfc_header(
                            &mut message_data.properties,
                            RfcHeader::Sender,
                            MailHeaderForm::Addresses,
                            false,
                        )?,
                        MailProperties::From => add_rfc_header(
                            &mut message_data.properties,
                            RfcHeader::From,
                            MailHeaderForm::Addresses,
                            false,
                        )?,
                        MailProperties::To => add_rfc_header(
                            &mut message_data.properties,
                            RfcHeader::To,
                            MailHeaderForm::Addresses,
                            false,
                        )?,
                        MailProperties::Cc => add_rfc_header(
                            &mut message_data.properties,
                            RfcHeader::Cc,
                            MailHeaderForm::Addresses,
                            false,
                        )?,
                        MailProperties::Bcc => add_rfc_header(
                            &mut message_data.properties,
                            RfcHeader::Bcc,
                            MailHeaderForm::Addresses,
                            false,
                        )?,
                        MailProperties::ReplyTo => add_rfc_header(
                            &mut message_data.properties,
                            RfcHeader::ReplyTo,
                            MailHeaderForm::Addresses,
                            false,
                        )?,
                        MailProperties::Subject => add_rfc_header(
                            &mut message_data.properties,
                            RfcHeader::Subject,
                            MailHeaderForm::Text,
                            false,
                        )?,
                        MailProperties::SentAt => add_rfc_header(
                            &mut message_data.properties,
                            RfcHeader::Date,
                            MailHeaderForm::Date,
                            false,
                        )?,
                        MailProperties::Header(MailHeaderProperty {
                            form,
                            header: HeaderName::Rfc(header),
                            all,
                        }) => add_rfc_header(
                            &mut message_data.properties,
                            *header,
                            form.clone(),
                            *all,
                        )?,
                        MailProperties::Id => JSONValue::String(jmap_id.to_jmap_string()),
                        MailProperties::BlobId => JSONValue::String(
                            BlobId::new_owned(
                                request.account_id,
                                Collection::Mail,
                                document_id,
                                MESSAGE_RAW,
                            )
                            .to_jmap_string(),
                        ),
                        MailProperties::ThreadId => {
                            JSONValue::String((jmap_id.get_prefix_id() as JMAPId).to_jmap_string())
                        }
                        MailProperties::MailboxIds => {
                            if let Some(mailboxes) = self.get_document_tags(
                                request.account_id,
                                Collection::Mail,
                                document_id,
                                MessageField::Mailbox.into(),
                            )? {
                                JSONValue::Object(
                                    mailboxes
                                        .items
                                        .into_iter()
                                        .map(|mailbox| {
                                            (
                                                (mailbox.unwrap_id().unwrap() as JMAPId)
                                                    .to_jmap_string(),
                                                JSONValue::Bool(true),
                                            )
                                        })
                                        .collect(),
                                )
                            } else {
                                JSONValue::Null
                            }
                        }
                        MailProperties::Keywords => {
                            if let Some(tags) = self.get_document_tags(
                                request.account_id,
                                Collection::Mail,
                                document_id,
                                MessageField::Keyword.into(),
                            )? {
                                let mut keywords = HashMap::with_capacity(tags.items.len());
                                for tag in tags.items {
                                    keywords.insert(Keyword::to_jmap(tag)?, true.into());
                                }
                                keywords.into()
                            } else {
                                JSONValue::Null
                            }
                        }
                        MailProperties::Size
                        | MailProperties::ReceivedAt
                        | MailProperties::HasAttachment => {
                            message_data.properties.remove(property).unwrap_or_default()
                        }
                        MailProperties::TextBody => add_body_parts(
                            &message_data.text_body,
                            &message_data.mime_parts,
                            &arguments.body_properties,
                            message_raw_ref,
                            message_outline.as_ref(),
                            &base_blob_id,
                        ),

                        MailProperties::HtmlBody => add_body_parts(
                            &message_data.html_body,
                            &message_data.mime_parts,
                            &arguments.body_properties,
                            message_raw_ref,
                            message_outline.as_ref(),
                            &base_blob_id,
                        ),

                        MailProperties::Attachments => add_body_parts(
                            &message_data.attachments,
                            &message_data.mime_parts,
                            &arguments.body_properties,
                            message_raw_ref,
                            message_outline.as_ref(),
                            &base_blob_id,
                        ),

                        MailProperties::Preview => {
                            if !message_data.text_body.is_empty() {
                                JSONValue::String(
                                    preview_text(
                                        String::from_utf8(
                                            self.get_blob_range(
                                                request.account_id,
                                                Collection::Mail,
                                                document_id,
                                                MESSAGE_PARTS
                                                    + message_data
                                                        .text_body
                                                        .get(0)
                                                        .and_then(|p| {
                                                            message_data.mime_parts.get(p + 1)
                                                        })
                                                        .ok_or(StoreError::DataCorruption)?
                                                        .blob_index,
                                                0..260,
                                            )?
                                            .ok_or(StoreError::DataCorruption)?,
                                        )
                                        .map_or_else(
                                            |err| {
                                                String::from_utf8_lossy(err.as_bytes()).into_owned()
                                            },
                                            |s| s,
                                        )
                                        .into(),
                                        256,
                                    )
                                    .to_string(),
                                )
                            } else if !message_data.html_body.is_empty() {
                                JSONValue::String(
                                    preview_html(
                                        String::from_utf8(
                                            self.get_blob(
                                                request.account_id,
                                                Collection::Mail,
                                                document_id,
                                                MESSAGE_PARTS
                                                    + message_data
                                                        .html_body
                                                        .get(0)
                                                        .and_then(|p| {
                                                            message_data.mime_parts.get(p + 1)
                                                        })
                                                        .ok_or(StoreError::DataCorruption)?
                                                        .blob_index,
                                            )?
                                            .ok_or(StoreError::DataCorruption)?,
                                        )
                                        .map_or_else(
                                            |err| {
                                                String::from_utf8_lossy(err.as_bytes()).into_owned()
                                            },
                                            |s| s,
                                        )
                                        .into(),
                                        256,
                                    )
                                    .to_string(),
                                )
                            } else {
                                JSONValue::Null
                            }
                        }
                        MailProperties::BodyValues => {
                            let mut fetch_parts = BTreeMap::new();
                            if arguments.fetch_all_body_values || arguments.fetch_text_body_values {
                                message_data.text_body.iter().for_each(|part| {
                                    if let Some(mime_part) = message_data.mime_parts.get(*part + 1)
                                    {
                                        if let MimePartType::Html | MimePartType::Text =
                                            mime_part.mime_type
                                        {
                                            if let btree_map::Entry::Vacant(entry) = fetch_parts
                                                .entry(mime_part.blob_index + MESSAGE_PARTS)
                                            {
                                                entry.insert((mime_part, *part));
                                            }
                                        }
                                    }
                                });
                            }
                            if arguments.fetch_all_body_values || arguments.fetch_html_body_values {
                                message_data.html_body.iter().for_each(|part| {
                                    if let Some(mime_part) = message_data.mime_parts.get(*part + 1)
                                    {
                                        if let MimePartType::Html | MimePartType::Text =
                                            mime_part.mime_type
                                        {
                                            if let btree_map::Entry::Vacant(entry) = fetch_parts
                                                .entry(mime_part.blob_index + MESSAGE_PARTS)
                                            {
                                                entry.insert((mime_part, *part));
                                            }
                                        }
                                    }
                                });
                            }

                            if !fetch_parts.is_empty() {
                                let blobs = fetch_parts
                                    .keys()
                                    .map(|k| {
                                        if arguments.max_body_value_bytes == 0 {
                                            (*k, 0..u32::MAX)
                                        } else {
                                            (*k, 0..(arguments.max_body_value_bytes + 10) as u32)
                                        }
                                    })
                                    .collect();
                                JSONValue::Object(HashMap::from_iter(
                                    self.get_blobs(
                                        request.account_id,
                                        Collection::Mail,
                                        document_id,
                                        blobs,
                                    )?
                                    .into_iter()
                                    .map(|blob_entry| {
                                        let (mime_part, part_id) =
                                            fetch_parts.get(&blob_entry.0).unwrap();

                                        (
                                            part_id.to_string(),
                                            add_body_value(
                                                mime_part,
                                                String::from_utf8(blob_entry.1).map_or_else(
                                                    |err| {
                                                        String::from_utf8_lossy(err.as_bytes())
                                                            .into_owned()
                                                    },
                                                    |s| s,
                                                ),
                                                &arguments,
                                            ),
                                        )
                                    }),
                                ))
                            } else {
                                JSONValue::Null
                            }
                        }
                        MailProperties::BodyStructure => {
                            if let Some(body_structure) = add_body_structure(
                                message_outline.as_ref().unwrap(),
                                &message_data.mime_parts,
                                &arguments.body_properties,
                                message_raw_ref,
                                &base_blob_id,
                            ) {
                                body_structure
                            } else {
                                JSONValue::Null
                            }
                        }
                    };

                    if !value.is_null() {
                        entry.insert(value);
                    }
                }
            }

            results.push(result.into());
        }

        let mut obj = HashMap::new();
        obj.insert(
            "state".to_string(),
            self.get_state(request.account_id, Collection::Mail)?.into(),
        );
        obj.insert("list".to_string(), results.into());
        obj.insert("notFound".to_string(), not_found.into());
        Ok(obj.into())
    }
}

pub fn add_body_value(
    mime_part: &MimePart,
    body_text: String,
    arguments: &MailGetArguments,
) -> JSONValue {
    let mut body_value = HashMap::with_capacity(3);
    body_value.insert(
        "isEncodingProblem".into(),
        JSONValue::Bool(mime_part.is_encoding_problem),
    );
    body_value.insert(
        "isTruncated".into(),
        JSONValue::Bool(
            arguments.max_body_value_bytes > 0 && body_text.len() > arguments.max_body_value_bytes,
        ),
    );
    body_value.insert(
        "value".into(),
        if arguments.max_body_value_bytes == 0 || body_text.len() <= arguments.max_body_value_bytes
        {
            JSONValue::String(body_text)
        } else {
            JSONValue::String(if let MimePartType::Html = mime_part.mime_type {
                truncate_html(body_text.into(), arguments.max_body_value_bytes).to_string()
            } else {
                truncate_text(body_text.into(), arguments.max_body_value_bytes).to_string()
            })
        },
    );
    body_value.into()
}

pub fn add_body_structure(
    message_outline: &MessageOutline,
    mime_parts: &[MimePart],
    properties: &[MailBodyProperties],
    message_raw: Option<&[u8]>,
    base_blob_id: &BlobId,
) -> Option<JSONValue> {
    let mut parts_stack = Vec::with_capacity(5);
    let mut stack = Vec::new();

    let part_list = match &message_outline.body_structure {
        MessageStructure::Part(part_id) => {
            return Some(JSONValue::Object(add_body_part(
                (*part_id).into(),
                mime_parts.get(part_id + 1)?,
                properties,
                message_raw,
                message_outline.headers.get(0),
                base_blob_id,
            )))
        }
        MessageStructure::List(part_list) => {
            parts_stack.push(add_body_part(
                None,
                mime_parts.get(0)?,
                properties,
                message_raw,
                message_outline.headers.get(0),
                base_blob_id,
            ));
            part_list
        }
        MessageStructure::MultiPart((part_id, part_list)) => {
            parts_stack.push(add_body_part(
                None,
                mime_parts.get(0)?,
                properties,
                message_raw,
                message_outline.headers.get(0),
                base_blob_id,
            ));
            parts_stack.push(add_body_part(
                None,
                mime_parts.get(part_id + 1)?,
                properties,
                message_raw,
                message_outline.headers.get(part_id + 1),
                base_blob_id,
            ));
            stack.push(([].iter(), vec![]));
            part_list
        }
    };

    let mut subparts = Vec::with_capacity(part_list.len());
    let mut part_list_iter = part_list.iter();

    loop {
        while let Some(part) = part_list_iter.next() {
            match part {
                MessageStructure::Part(part_id) => subparts.push(JSONValue::Object(add_body_part(
                    (*part_id).into(),
                    mime_parts.get(part_id + 1)?,
                    properties,
                    message_raw,
                    message_outline.headers.get(part_id + 1),
                    base_blob_id,
                ))),
                MessageStructure::MultiPart((part_id, next_part_list)) => {
                    parts_stack.push(add_body_part(
                        None,
                        mime_parts.get(part_id + 1)?,
                        properties,
                        message_raw,
                        message_outline.headers.get(part_id + 1),
                        base_blob_id,
                    ));
                    stack.push((part_list_iter, subparts));
                    part_list_iter = next_part_list.iter();
                    subparts = Vec::with_capacity(part_list.len());
                }
                MessageStructure::List(_) => (),
            }
        }

        if let Some((prev_part_list_iter, mut prev_subparts)) = stack.pop() {
            let mut prev_part = parts_stack.pop().unwrap();
            prev_part.insert("subParts".into(), JSONValue::Array(subparts));
            prev_subparts.push(JSONValue::Object(prev_part));
            part_list_iter = prev_part_list_iter;
            subparts = prev_subparts;
        } else {
            break;
        }
    }

    let mut root_part = parts_stack.pop().unwrap();
    root_part.insert("subParts".into(), JSONValue::Array(subparts));
    Some(JSONValue::Object(root_part))
}

pub fn add_body_parts(
    parts: &[usize],
    mime_parts: &[MimePart],
    properties: &[MailBodyProperties],
    message_raw: Option<&[u8]>,
    message_outline: Option<&MessageOutline>,
    base_blob_id: &BlobId,
) -> JSONValue {
    JSONValue::Array(
        parts
            .iter()
            .filter_map(|part_id| {
                Some(JSONValue::Object(add_body_part(
                    (*part_id).into(),
                    mime_parts.get(part_id + 1)?,
                    properties,
                    message_raw,
                    if let Some(message_outline) = message_outline {
                        message_outline.headers.get(part_id + 1)
                    } else {
                        None
                    },
                    base_blob_id,
                )))
            })
            .collect(),
    )
}

fn add_body_part(
    part_id: Option<usize>,
    mime_part: &MimePart,
    properties: &[MailBodyProperties],
    message_raw: Option<&[u8]>,
    headers_raw: Option<&HashMap<HeaderName, Vec<HeaderOffset>>>,
    base_blob_id: &BlobId,
) -> HashMap<String, JSONValue> {
    let mut body_part = HashMap::with_capacity(properties.len());
    let mut headers_result: HashMap<String, Vec<JSONValue>> = HashMap::new();
    let has_raw_headers = headers_raw.is_some();

    let get_raw_header = |value: &Vec<HeaderOffset>| -> Vec<_> {
        value
            .iter()
            .filter_map(|offset| {
                Some(JSONValue::String(
                    std::str::from_utf8(message_raw.unwrap().get(offset.start..offset.end)?)
                        .map_or_else(
                            |_| {
                                String::from_utf8_lossy(
                                    message_raw.unwrap().get(offset.start..offset.end).unwrap(),
                                )
                                .trim()
                                .to_string()
                            },
                            |str| str.trim().to_string(),
                        ),
                ))
            })
            .collect::<Vec<JSONValue>>()
    };

    for property in properties {
        match property {
            MailBodyProperties::Size
            | MailBodyProperties::Name
            | MailBodyProperties::Type
            | MailBodyProperties::Charset
            | MailBodyProperties::Disposition
            | MailBodyProperties::Cid
            | MailBodyProperties::Language
            | MailBodyProperties::Location => {
                if let Some(value) = mime_part.headers.get(property) {
                    body_part.insert(property.to_string(), value.clone());
                }
            }

            MailBodyProperties::BlobId if part_id.is_some() => {
                body_part.insert(
                    "blobId".into(),
                    JSONValue::String(
                        base_blob_id
                            .clone_with_index(mime_part.blob_index)
                            .to_jmap_string(),
                    ),
                );
            }
            MailBodyProperties::Header(header) if has_raw_headers => {
                if let Some(offsets) = headers_raw.unwrap().get(&header.header) {
                    body_part.insert(
                        header.to_string(),
                        add_raw_header(
                            offsets,
                            message_raw.as_ref().unwrap(),
                            header.form.clone(),
                            header.all,
                        ),
                    );
                }
            }
            MailBodyProperties::Headers if has_raw_headers => {
                for (header, value) in headers_raw.unwrap() {
                    if let Entry::Vacant(entry) = headers_result.entry(header.as_str().to_string())
                    {
                        entry.insert(get_raw_header(value));
                    }
                }
            }
            MailBodyProperties::PartId => {
                if let Some(part_id) = part_id {
                    body_part.insert("partId".into(), part_id.into());
                }
            }
            _ => (),
        }
    }

    if !headers_result.is_empty() {
        body_part.insert(
            "headers".into(),
            JSONValue::Array(
                headers_result
                    .into_iter()
                    .flat_map(|(header, values)| {
                        values.into_iter().map(move |value| {
                            let mut result: HashMap<String, JSONValue> = HashMap::with_capacity(2);
                            result.insert("name".into(), JSONValue::String((&header).clone()));
                            result.insert("value".into(), value);
                            JSONValue::Object(result)
                        })
                    })
                    .collect(),
            ),
        );
    }

    body_part
}

fn add_rfc_header(
    message_headers: &mut JMAPMailHeaders,
    header: RfcHeader,
    form: MailHeaderForm,
    all: bool,
) -> jmap::Result<JSONValue> {
    let (value, is_collection, is_grouped) = match &form {
        MailHeaderForm::Addresses | MailHeaderForm::GroupedAddresses => {
            if let Some(value) = message_headers.remove(&MailProperties::Header(
                MailHeaderProperty::new_rfc(header, MailHeaderForm::Addresses, false),
            )) {
                (value, false, false)
            } else if let Some(value) = message_headers.remove(&MailProperties::Header(
                MailHeaderProperty::new_rfc(header, MailHeaderForm::Addresses, true),
            )) {
                (value, true, false)
            } else if let Some(value) = message_headers.remove(&MailProperties::Header(
                MailHeaderProperty::new_rfc(header, MailHeaderForm::GroupedAddresses, false),
            )) {
                (value, false, true)
            } else if let Some(value) = message_headers.remove(&MailProperties::Header(
                MailHeaderProperty::new_rfc(header, MailHeaderForm::GroupedAddresses, true),
            )) {
                (value, true, true)
            } else {
                (JSONValue::Null, false, false)
            }
        }
        _ => {
            if let Some(value) = message_headers.remove(&MailProperties::Header(
                MailHeaderProperty::new_rfc(header, form.clone(), all),
            )) {
                (value, all, false)
            } else if let Some(value) = message_headers.remove(&MailProperties::Header(
                MailHeaderProperty::new_rfc(header, form.clone(), !all),
            )) {
                (value, !all, false)
            } else {
                (JSONValue::Null, false, false)
            }
        }
    };

    transform_rfc_header(header, value, form, is_collection, is_grouped, all)
}

pub fn transform_rfc_header(
    header: RfcHeader,
    value: JSONValue,
    form: MailHeaderForm,
    is_collection: bool,
    is_grouped: bool,
    as_collection: bool,
) -> jmap::Result<JSONValue> {
    Ok(match (header, form.clone()) {
        (RfcHeader::Date | RfcHeader::ResentDate, MailHeaderForm::Date)
        | (
            RfcHeader::Subject | RfcHeader::Comments | RfcHeader::Keywords | RfcHeader::ListId,
            MailHeaderForm::Text,
        ) => transform_json_string(value, as_collection),
        (
            RfcHeader::MessageId
            | RfcHeader::References
            | RfcHeader::ResentMessageId
            | RfcHeader::InReplyTo,
            MailHeaderForm::MessageIds,
        )
        | (
            RfcHeader::ListArchive
            | RfcHeader::ListHelp
            | RfcHeader::ListOwner
            | RfcHeader::ListPost
            | RfcHeader::ListSubscribe
            | RfcHeader::ListUnsubscribe,
            MailHeaderForm::URLs,
        ) => transform_json_stringlist(value, is_collection, as_collection),
        (
            RfcHeader::From
            | RfcHeader::To
            | RfcHeader::Cc
            | RfcHeader::Bcc
            | RfcHeader::ReplyTo
            | RfcHeader::Sender
            | RfcHeader::ResentTo
            | RfcHeader::ResentFrom
            | RfcHeader::ResentBcc
            | RfcHeader::ResentCc
            | RfcHeader::ResentSender,
            MailHeaderForm::Addresses | MailHeaderForm::GroupedAddresses,
        ) => transform_json_emailaddress(
            value,
            is_grouped,
            is_collection,
            matches!(form, MailHeaderForm::GroupedAddresses),
            as_collection,
        ),
        _ => {
            return Err(JMAPError::InvalidArguments(
                "Invalid header property.".to_string(),
            ))
        }
    })
}

pub fn add_raw_header(
    offsets: &[HeaderOffset],
    message_raw: &[u8],
    form: MailHeaderForm,
    all: bool,
) -> JSONValue {
    let mut header_values: Vec<HeaderValue> = offsets
        .iter()
        .skip(if !all && offsets.len() > 1 {
            offsets.len() - 1
        } else {
            0
        })
        .map(|offset| {
            (message_raw.get(offset.start..offset.end).map_or(
                HeaderValue::Empty,
                |bytes| match form {
                    MailHeaderForm::Raw => {
                        HeaderValue::Text(std::str::from_utf8(bytes).map_or_else(
                            |_| String::from_utf8_lossy(bytes).trim().to_string().into(),
                            |str| str.trim().to_string().into(),
                        ))
                    }
                    MailHeaderForm::Text => parse_unstructured(&mut MessageStream::new(bytes)),
                    MailHeaderForm::Addresses => parse_address(&mut MessageStream::new(bytes)),
                    MailHeaderForm::GroupedAddresses => {
                        parse_address(&mut MessageStream::new(bytes))
                    }
                    MailHeaderForm::MessageIds => parse_id(&mut MessageStream::new(bytes)),
                    MailHeaderForm::Date => parse_date(&mut MessageStream::new(bytes)),
                    MailHeaderForm::URLs => parse_address(&mut MessageStream::new(bytes)),
                },
            ))
            .into_owned()
        })
        .collect();
    let header_values = if all {
        HeaderValue::Collection(header_values)
    } else {
        header_values.pop().unwrap_or_default()
    };
    match form {
        MailHeaderForm::Raw | MailHeaderForm::Text => {
            let (value, _) = header_to_jmap_text(header_values);
            value
        }
        MailHeaderForm::Addresses | MailHeaderForm::GroupedAddresses => {
            let (value, is_grouped, is_collection) = header_to_jmap_address(header_values, false);
            transform_json_emailaddress(
                value,
                is_grouped,
                is_collection,
                matches!(form, MailHeaderForm::GroupedAddresses),
                all,
            )
        }
        MailHeaderForm::MessageIds => {
            let (value, _) = header_to_jmap_id(header_values);
            value
        }
        MailHeaderForm::Date => {
            let (value, _) = header_to_jmap_date(header_values);
            value
        }
        MailHeaderForm::URLs => {
            let (value, _) = header_to_jmap_url(header_values);
            value
        }
    }
}

pub fn transform_json_emailaddress(
    value: JSONValue,
    is_grouped: bool,
    is_collection: bool,
    as_grouped: bool,
    as_collection: bool,
) -> JSONValue {
    if let JSONValue::Array(mut list) = value {
        if ((as_grouped && is_grouped) || (!as_grouped && !is_grouped))
            && ((is_collection && as_collection) || (!is_collection && !as_collection))
        {
            JSONValue::Array(list)
        } else if (as_grouped && is_grouped) || (!as_grouped && !is_grouped) {
            if as_collection && !is_collection {
                JSONValue::Array(vec![JSONValue::Array(list)])
            } else {
                // !as_collection && is_collection
                list.pop().unwrap_or_default()
            }
        } else {
            let mut list = if as_collection && !is_collection {
                vec![JSONValue::Array(list)]
            } else if !as_collection && is_collection {
                if let JSONValue::Array(list) = list.pop().unwrap_or_default() {
                    list
                } else {
                    vec![]
                }
            } else {
                list
            };

            if as_grouped && !is_grouped {
                let list_to_group = |list: Vec<JSONValue>| -> JSONValue {
                    let mut group = HashMap::new();
                    group.insert("name".to_string(), JSONValue::Null);
                    group.insert("addresses".to_string(), JSONValue::Array(list));
                    JSONValue::Object(group)
                };
                JSONValue::Array(if !as_collection {
                    vec![list_to_group(list)]
                } else {
                    list.iter_mut().for_each(|field| {
                        if let JSONValue::Array(list) = field {
                            *field = JSONValue::Array(vec![list_to_group(std::mem::take(list))]);
                        }
                    });
                    list
                })
            } else {
                // !as_grouped && is_grouped
                let flatten_group = |list: Vec<JSONValue>| -> Vec<JSONValue> {
                    let mut addresses = Vec::with_capacity(list.len() * 2);
                    list.into_iter().for_each(|group| {
                        if let JSONValue::Object(mut group) = group {
                            if let Some(JSONValue::Array(mut group_addresses)) =
                                group.remove("addresses")
                            {
                                addresses.append(&mut group_addresses);
                            }
                        }
                    });
                    addresses
                };
                JSONValue::Array(if !as_collection {
                    flatten_group(list)
                } else {
                    list.into_iter()
                        .map(|field| {
                            if let JSONValue::Array(list) = field {
                                JSONValue::Array(flatten_group(list))
                            } else {
                                field
                            }
                        })
                        .collect()
                })
            }
        }
    } else {
        JSONValue::Null
    }
}

pub fn transform_json_stringlist(
    value: JSONValue,
    is_collection: bool,
    as_collection: bool,
) -> JSONValue {
    if let JSONValue::Array(mut list) = value {
        if !as_collection {
            if !is_collection {
                JSONValue::Array(list)
            } else {
                list.pop().unwrap_or_default()
            }
        } else if is_collection {
            JSONValue::Array(list)
        } else {
            JSONValue::Array(vec![JSONValue::Array(list)])
        }
    } else {
        JSONValue::Null
    }
}

pub fn transform_json_string(value: JSONValue, as_collection: bool) -> JSONValue {
    match value {
        JSONValue::Array(mut list) => {
            if !as_collection {
                list.pop().unwrap_or_default()
            } else {
                JSONValue::Array(list)
            }
        }
        value @ JSONValue::String(_) => {
            if !as_collection {
                value
            } else {
                JSONValue::Array(vec![value])
            }
        }
        _ => JSONValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use jmap::json::JSONValue;

    #[test]
    fn test_json_transform() {
        for (value, expected_result, expected_result_all) in [
            (
                JSONValue::String("hello".into()),
                JSONValue::String("hello".into()),
                JSONValue::Array(vec![JSONValue::String("hello".into())]),
            ),
            (
                JSONValue::Array(vec![
                    JSONValue::String("hello".into()),
                    JSONValue::String("world".into()),
                ]),
                JSONValue::String("world".into()),
                JSONValue::Array(vec![
                    JSONValue::String("hello".into()),
                    JSONValue::String("world".into()),
                ]),
            ),
        ] {
            assert_eq!(
                super::transform_json_string(value.clone(), false),
                expected_result
            );
            assert_eq!(
                super::transform_json_string(value, true),
                expected_result_all
            );
        }

        for (value, is_collection, expected_result, expected_result_all) in [
            (
                JSONValue::Array(vec![
                    JSONValue::String("hello".into()),
                    JSONValue::String("world".into()),
                ]),
                false,
                JSONValue::Array(vec![
                    JSONValue::String("hello".into()),
                    JSONValue::String("world".into()),
                ]),
                JSONValue::Array(vec![JSONValue::Array(vec![
                    JSONValue::String("hello".into()),
                    JSONValue::String("world".into()),
                ])]),
            ),
            (
                JSONValue::Array(vec![
                    JSONValue::Array(vec![
                        JSONValue::String("hello".into()),
                        JSONValue::String("world".into()),
                    ]),
                    JSONValue::Array(vec![
                        JSONValue::String("hola".into()),
                        JSONValue::String("mundo".into()),
                    ]),
                ]),
                true,
                JSONValue::Array(vec![
                    JSONValue::String("hola".into()),
                    JSONValue::String("mundo".into()),
                ]),
                JSONValue::Array(vec![
                    JSONValue::Array(vec![
                        JSONValue::String("hello".into()),
                        JSONValue::String("world".into()),
                    ]),
                    JSONValue::Array(vec![
                        JSONValue::String("hola".into()),
                        JSONValue::String("mundo".into()),
                    ]),
                ]),
            ),
        ] {
            assert_eq!(
                super::transform_json_stringlist(value.clone(), is_collection, false),
                expected_result
            );
            assert_eq!(
                super::transform_json_stringlist(value, is_collection, true),
                expected_result_all
            );
        }

        fn make_email(name: &str, addr: &str) -> JSONValue {
            let mut email = HashMap::new();
            email.insert("name".to_string(), JSONValue::String(name.to_string()));
            email.insert("email".to_string(), JSONValue::String(addr.to_string()));
            JSONValue::Object(email)
        }

        fn make_group(name: Option<&str>, addresses: JSONValue) -> JSONValue {
            let mut email = HashMap::new();
            email.insert(
                "name".to_string(),
                name.map_or(JSONValue::Null, |name| JSONValue::String(name.to_string())),
            );
            email.insert("addresses".to_string(), addresses);
            JSONValue::Object(email)
        }

        fn make_list(value1: JSONValue, value2: JSONValue) -> JSONValue {
            JSONValue::Array(vec![value1, value2])
        }

        fn make_list_many(
            value1: JSONValue,
            value2: JSONValue,
            value3: JSONValue,
            value4: JSONValue,
        ) -> JSONValue {
            JSONValue::Array(vec![value1, value2, value3, value4])
        }

        fn make_list_single(value: JSONValue) -> JSONValue {
            JSONValue::Array(vec![value])
        }

        for (
            value,
            is_grouped,
            is_collection,
            expected_result_single_addr,
            expected_result_all_addr,
            expected_result_single_group,
            expected_result_all_group,
        ) in [
            (
                make_list(
                    make_email("John Doe", "jdoe@domain.com"),
                    make_email("Jane Smith", "jsmith@test.com"),
                ),
                false,
                false,
                make_list(
                    make_email("John Doe", "jdoe@domain.com"),
                    make_email("Jane Smith", "jsmith@test.com"),
                ),
                make_list_single(make_list(
                    make_email("John Doe", "jdoe@domain.com"),
                    make_email("Jane Smith", "jsmith@test.com"),
                )),
                make_list_single(make_group(
                    None,
                    make_list(
                        make_email("John Doe", "jdoe@domain.com"),
                        make_email("Jane Smith", "jsmith@test.com"),
                    ),
                )),
                make_list_single(make_list_single(make_group(
                    None,
                    make_list(
                        make_email("John Doe", "jdoe@domain.com"),
                        make_email("Jane Smith", "jsmith@test.com"),
                    ),
                ))),
            ),
            (
                make_list(
                    make_list(
                        make_email("John Doe", "jdoe@domain.com"),
                        make_email("Jane Smith", "jsmith@test.com"),
                    ),
                    make_list(
                        make_email("Juan Gomez", "jgomez@dominio.com"),
                        make_email("Juanita Perez", "jperez@prueba.com"),
                    ),
                ),
                false,
                true,
                make_list(
                    make_email("Juan Gomez", "jgomez@dominio.com"),
                    make_email("Juanita Perez", "jperez@prueba.com"),
                ),
                make_list(
                    make_list(
                        make_email("John Doe", "jdoe@domain.com"),
                        make_email("Jane Smith", "jsmith@test.com"),
                    ),
                    make_list(
                        make_email("Juan Gomez", "jgomez@dominio.com"),
                        make_email("Juanita Perez", "jperez@prueba.com"),
                    ),
                ),
                make_list_single(make_group(
                    None,
                    make_list(
                        make_email("Juan Gomez", "jgomez@dominio.com"),
                        make_email("Juanita Perez", "jperez@prueba.com"),
                    ),
                )),
                make_list(
                    make_list_single(make_group(
                        None,
                        make_list(
                            make_email("John Doe", "jdoe@domain.com"),
                            make_email("Jane Smith", "jsmith@test.com"),
                        ),
                    )),
                    make_list_single(make_group(
                        None,
                        make_list(
                            make_email("Juan Gomez", "jgomez@dominio.com"),
                            make_email("Juanita Perez", "jperez@prueba.com"),
                        ),
                    )),
                ),
            ),
            (
                make_list(
                    make_group(
                        "Group 1".into(),
                        make_list(
                            make_email("John Doe", "jdoe@domain.com"),
                            make_email("Jane Smith", "jsmith@test.com"),
                        ),
                    ),
                    make_group(
                        "Group 2".into(),
                        make_list(
                            make_email("Juan Gomez", "jgomez@dominio.com"),
                            make_email("Juanita Perez", "jperez@prueba.com"),
                        ),
                    ),
                ),
                true,
                false,
                make_list_many(
                    make_email("John Doe", "jdoe@domain.com"),
                    make_email("Jane Smith", "jsmith@test.com"),
                    make_email("Juan Gomez", "jgomez@dominio.com"),
                    make_email("Juanita Perez", "jperez@prueba.com"),
                ),
                make_list_single(make_list_many(
                    make_email("John Doe", "jdoe@domain.com"),
                    make_email("Jane Smith", "jsmith@test.com"),
                    make_email("Juan Gomez", "jgomez@dominio.com"),
                    make_email("Juanita Perez", "jperez@prueba.com"),
                )),
                make_list(
                    make_group(
                        "Group 1".into(),
                        make_list(
                            make_email("John Doe", "jdoe@domain.com"),
                            make_email("Jane Smith", "jsmith@test.com"),
                        ),
                    ),
                    make_group(
                        "Group 2".into(),
                        make_list(
                            make_email("Juan Gomez", "jgomez@dominio.com"),
                            make_email("Juanita Perez", "jperez@prueba.com"),
                        ),
                    ),
                ),
                make_list_single(make_list(
                    make_group(
                        "Group 1".into(),
                        make_list(
                            make_email("John Doe", "jdoe@domain.com"),
                            make_email("Jane Smith", "jsmith@test.com"),
                        ),
                    ),
                    make_group(
                        "Group 2".into(),
                        make_list(
                            make_email("Juan Gomez", "jgomez@dominio.com"),
                            make_email("Juanita Perez", "jperez@prueba.com"),
                        ),
                    ),
                )),
            ),
            (
                make_list(
                    make_list(
                        make_group(
                            "Group 1".into(),
                            make_list(
                                make_email("Tim Hortons", "tim@hortos.com"),
                                make_email("Ronald McDowell", "ronnie@mac.com"),
                            ),
                        ),
                        make_group(
                            "Group 2".into(),
                            make_list(
                                make_email("Wendy D", "wendy@d.com"),
                                make_email("Kentucky Frango", "kentucky@frango.com"),
                            ),
                        ),
                    ),
                    make_list(
                        make_group(
                            "Group 3".into(),
                            make_list(
                                make_email("John Doe", "jdoe@domain.com"),
                                make_email("Jane Smith", "jsmith@test.com"),
                            ),
                        ),
                        make_group(
                            "Group 4".into(),
                            make_list(
                                make_email("Juan Gomez", "jgomez@dominio.com"),
                                make_email("Juanita Perez", "jperez@prueba.com"),
                            ),
                        ),
                    ),
                ),
                true,
                true,
                make_list_many(
                    make_email("John Doe", "jdoe@domain.com"),
                    make_email("Jane Smith", "jsmith@test.com"),
                    make_email("Juan Gomez", "jgomez@dominio.com"),
                    make_email("Juanita Perez", "jperez@prueba.com"),
                ),
                make_list(
                    make_list_many(
                        make_email("Tim Hortons", "tim@hortos.com"),
                        make_email("Ronald McDowell", "ronnie@mac.com"),
                        make_email("Wendy D", "wendy@d.com"),
                        make_email("Kentucky Frango", "kentucky@frango.com"),
                    ),
                    make_list_many(
                        make_email("John Doe", "jdoe@domain.com"),
                        make_email("Jane Smith", "jsmith@test.com"),
                        make_email("Juan Gomez", "jgomez@dominio.com"),
                        make_email("Juanita Perez", "jperez@prueba.com"),
                    ),
                ),
                make_list(
                    make_group(
                        "Group 3".into(),
                        make_list(
                            make_email("John Doe", "jdoe@domain.com"),
                            make_email("Jane Smith", "jsmith@test.com"),
                        ),
                    ),
                    make_group(
                        "Group 4".into(),
                        make_list(
                            make_email("Juan Gomez", "jgomez@dominio.com"),
                            make_email("Juanita Perez", "jperez@prueba.com"),
                        ),
                    ),
                ),
                make_list(
                    make_list(
                        make_group(
                            "Group 1".into(),
                            make_list(
                                make_email("Tim Hortons", "tim@hortos.com"),
                                make_email("Ronald McDowell", "ronnie@mac.com"),
                            ),
                        ),
                        make_group(
                            "Group 2".into(),
                            make_list(
                                make_email("Wendy D", "wendy@d.com"),
                                make_email("Kentucky Frango", "kentucky@frango.com"),
                            ),
                        ),
                    ),
                    make_list(
                        make_group(
                            "Group 3".into(),
                            make_list(
                                make_email("John Doe", "jdoe@domain.com"),
                                make_email("Jane Smith", "jsmith@test.com"),
                            ),
                        ),
                        make_group(
                            "Group 4".into(),
                            make_list(
                                make_email("Juan Gomez", "jgomez@dominio.com"),
                                make_email("Juanita Perez", "jperez@prueba.com"),
                            ),
                        ),
                    ),
                ),
            ),
        ] {
            assert_eq!(
                super::transform_json_emailaddress(
                    value.clone(),
                    is_grouped,
                    is_collection,
                    false,
                    false
                ),
                expected_result_single_addr,
                "single+address"
            );
            assert_eq!(
                super::transform_json_emailaddress(
                    value.clone(),
                    is_grouped,
                    is_collection,
                    false,
                    true
                ),
                expected_result_all_addr,
                "all+address"
            );
            assert_eq!(
                super::transform_json_emailaddress(
                    value.clone(),
                    is_grouped,
                    is_collection,
                    true,
                    false
                ),
                expected_result_single_group,
                "single+group"
            );
            assert_eq!(
                super::transform_json_emailaddress(
                    value.clone(),
                    is_grouped,
                    is_collection,
                    true,
                    true
                ),
                expected_result_all_group,
                "all+group"
            );
        }
    }
}