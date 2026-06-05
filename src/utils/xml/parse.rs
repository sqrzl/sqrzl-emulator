use crate::models::policy::{Grant, Grantee, Permission};
use crate::models::{Acl, CannedAcl};
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;

pub fn parse_versioning_xml(body: &str) -> Result<bool, String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_status = false;
    let mut enabled = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"Status" => {
                in_status = true;
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"Status" => {
                in_status = false;
            }
            Ok(Event::Text(e)) if in_status => {
                let decoded = e.decode().map_err(|err| err.to_string())?;
                let text = unescape(&decoded)
                    .map_err(|err| err.to_string())?
                    .to_string();
                enabled = text == "Enabled";
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    Ok(enabled)
}

pub fn parse_tagging_xml(body: &str) -> Result<HashMap<String, String>, String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_key = false;
    let mut in_value = false;
    let mut current_key: Option<String> = None;
    let mut tags = HashMap::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"Key" => in_key = true,
                b"Value" => in_value = true,
                _ => {}
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"Key" => in_key = false,
                b"Value" => in_value = false,
                _ => {}
            },
            Ok(Event::Text(e)) => {
                let decoded = e.decode().map_err(|err| err.to_string())?;
                let text = unescape(&decoded)
                    .map_err(|err| err.to_string())?
                    .to_string();
                if in_key {
                    current_key = Some(text);
                } else if in_value {
                    if let Some(k) = current_key.take() {
                        tags.insert(k, text);
                    } else {
                        return Err("InvalidTagKey".to_string());
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
        buf.clear();
    }

    if tags.len() > 10 {
        return Err("TooManyTags".to_string());
    }

    for (k, v) in tags.iter() {
        if k.is_empty() {
            return Err("InvalidTagKey".to_string());
        }
        if k.len() > 128 {
            return Err("InvalidTagKey".to_string());
        }
        if v.len() > 256 {
            return Err("InvalidTagValue".to_string());
        }
    }

    Ok(tags)
}

pub fn parse_acl_xml(body: &str) -> Result<Acl, String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut grants = Vec::new();
    let mut in_grant = false;
    let mut current_tag: Option<Vec<u8>> = None;
    let mut current_id: Option<String> = None;
    let mut current_display_name: Option<String> = None;
    let mut current_uri: Option<String> = None;
    let mut current_permission: Option<Permission> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.name().as_ref().to_vec();
                if name.as_slice() == b"Grant" {
                    in_grant = true;
                    current_id = None;
                    current_display_name = None;
                    current_uri = None;
                    current_permission = None;
                }
                current_tag = Some(name);
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"Grant" {
                    let permission = current_permission
                        .take()
                        .ok_or_else(|| "Missing ACL permission".to_string())?;
                    let grantee = if let Some(id) = current_id.take() {
                        Grantee::CanonicalUser {
                            id,
                            display_name: current_display_name.take(),
                        }
                    } else if let Some(uri) = current_uri.take() {
                        Grantee::Group { uri }
                    } else {
                        return Err("Missing ACL grantee".to_string());
                    };
                    grants.push(Grant {
                        grantee,
                        permission,
                    });
                    in_grant = false;
                }
                current_tag = None;
            }
            Ok(Event::Text(e)) => {
                if !in_grant {
                    buf.clear();
                    continue;
                }

                let decoded = e.decode().map_err(|err| err.to_string())?;
                let text = unescape(&decoded)
                    .map_err(|err| err.to_string())?
                    .to_string();
                match current_tag.as_deref() {
                    Some(b"ID") => current_id = Some(text),
                    Some(b"DisplayName") => current_display_name = Some(text),
                    Some(b"URI") => current_uri = Some(text),
                    Some(b"Permission") => {
                        current_permission = Some(match text.as_str() {
                            "READ" => Permission::Read,
                            "WRITE" => Permission::Write,
                            "READ_ACP" => Permission::ReadAcp,
                            "WRITE_ACP" => Permission::WriteAcp,
                            "FULL_CONTROL" => Permission::FullControl,
                            _ => return Err(format!("Unsupported ACL permission: {}", text)),
                        })
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    Ok(Acl {
        canned: CannedAcl::Private,
        grants,
    })
}

pub fn parse_lifecycle_xml(
    body: &str,
) -> Result<crate::models::lifecycle::LifecycleConfiguration, String> {
    use crate::models::lifecycle::*;

    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut rules = Vec::new();
    let mut current_rule: Option<Rule> = None;
    let mut current_filter: Option<Filter> = None;
    let mut current_expiration: Option<Expiration> = None;
    let mut current_noncurrent_version_expiration: Option<NoncurrentVersionExpiration> = None;
    let mut current_transition: Option<Transition> = None;
    let mut current_tag: Option<(String, String)> = None;

    let mut path_stack: Vec<String> = Vec::new();
    let mut text_buffer = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                path_stack.push(name.clone());

                match name.as_str() {
                    "Rule" => {
                        current_rule = Some(Rule {
                            id: None,
                            status: Status::Disabled,
                            filter: None,
                            expiration: None,
                            noncurrent_version_expiration: None,
                            transitions: Vec::new(),
                        });
                    }
                    "Filter" => {
                        current_filter = Some(Filter {
                            prefix: None,
                            tags: Vec::new(),
                        });
                    }
                    "Expiration" => {
                        current_expiration = Some(Expiration {
                            days: None,
                            date: None,
                            expired_object_delete_marker: None,
                        });
                    }
                    "NoncurrentVersionExpiration" => {
                        current_noncurrent_version_expiration =
                            Some(NoncurrentVersionExpiration { noncurrent_days: 0 });
                    }
                    "Transition" => {
                        current_transition = Some(Transition {
                            days: None,
                            date: None,
                            storage_class: StorageClass::Standard,
                        });
                    }
                    "Tag" => {
                        current_tag = Some((String::new(), String::new()));
                    }
                    _ => {}
                }
                text_buffer.clear();
            }
            Ok(Event::Text(e)) => {
                let decoded = e.decode().unwrap_or_default();
                text_buffer = unescape(&decoded).unwrap_or_default().to_string();
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();

                match name.as_str() {
                    "ID" => {
                        if let Some(ref mut rule) = current_rule {
                            rule.id = Some(text_buffer.clone());
                        }
                    }
                    "Status" => {
                        if let Some(ref mut rule) = current_rule {
                            rule.status = if text_buffer == "Enabled" {
                                Status::Enabled
                            } else {
                                Status::Disabled
                            };
                        }
                    }
                    "Prefix" => {
                        if let Some(ref mut filter) = current_filter {
                            filter.prefix = Some(text_buffer.clone());
                        }
                    }
                    "Key" => {
                        if let Some(ref mut tag) = current_tag {
                            tag.0 = text_buffer.clone();
                        }
                    }
                    "Value" => {
                        if let Some(ref mut tag) = current_tag {
                            tag.1 = text_buffer.clone();
                        }
                    }
                    "Tag" => {
                        if let (Some(ref mut filter), Some(tag)) =
                            (&mut current_filter, current_tag.take())
                        {
                            filter.tags.push(Tag {
                                key: tag.0,
                                value: tag.1,
                            });
                        }
                    }
                    "Days" => {
                        if let Ok(days) = text_buffer.parse::<u32>() {
                            if let Some(ref mut exp) = current_expiration {
                                exp.days = Some(days);
                            } else if let Some(ref mut trans) = current_transition {
                                trans.days = Some(days);
                            }
                        }
                    }
                    "NoncurrentDays" => {
                        if let Ok(days) = text_buffer.parse::<u32>() {
                            if let Some(ref mut noncurrent_expiration) =
                                current_noncurrent_version_expiration
                            {
                                noncurrent_expiration.noncurrent_days = days;
                            }
                        }
                    }
                    "Date" => {
                        if let Some(ref mut exp) = current_expiration {
                            exp.date = Some(text_buffer.clone());
                        } else if let Some(ref mut trans) = current_transition {
                            trans.date = Some(text_buffer.clone());
                        }
                    }
                    "ExpiredObjectDeleteMarker" => {
                        if let Some(ref mut exp) = current_expiration {
                            exp.expired_object_delete_marker = Some(text_buffer == "true");
                        }
                    }
                    "StorageClass" => {
                        if let Some(ref mut trans) = current_transition {
                            trans.storage_class = match text_buffer.as_str() {
                                "GLACIER" => StorageClass::Glacier,
                                "DEEP_ARCHIVE" => StorageClass::DeepArchive,
                                _ => StorageClass::Standard,
                            };
                        }
                    }
                    "Filter" => {
                        if let (Some(ref mut rule), Some(filter)) =
                            (&mut current_rule, current_filter.take())
                        {
                            rule.filter = Some(filter);
                        }
                    }
                    "Expiration" => {
                        if let (Some(ref mut rule), Some(exp)) =
                            (&mut current_rule, current_expiration.take())
                        {
                            rule.expiration = Some(exp);
                        }
                    }
                    "NoncurrentVersionExpiration" => {
                        if let (Some(ref mut rule), Some(noncurrent_expiration)) = (
                            &mut current_rule,
                            current_noncurrent_version_expiration.take(),
                        ) {
                            rule.noncurrent_version_expiration = Some(noncurrent_expiration);
                        }
                    }
                    "Transition" => {
                        if let (Some(ref mut rule), Some(trans)) =
                            (&mut current_rule, current_transition.take())
                        {
                            rule.transitions.push(trans);
                        }
                    }
                    "Rule" => {
                        if let Some(rule) = current_rule.take() {
                            rules.push(rule);
                        }
                    }
                    _ => {}
                }

                path_stack.pop();
                text_buffer.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {}", e)),
            _ => {}
        }
    }

    Ok(LifecycleConfiguration { rules })
}
