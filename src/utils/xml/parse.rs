use crate::models::lifecycle::{
    Expiration, Filter, LifecycleConfiguration, NoncurrentVersionExpiration, Rule, Status,
    StorageClass, Tag, Transition,
};
use crate::models::policy::{Grant, Grantee, Permission};
use crate::models::{Acl, CannedAcl};
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
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
            Err(e) => return Err(format!("XML parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(enabled)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
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

    for (k, v) in &tags {
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

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
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
                            _ => return Err(format!("Unsupported ACL permission: {text}")),
                        });
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(Acl {
        canned: CannedAcl::Private,
        grants,
    })
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn parse_lifecycle_xml(
    body: &str,
) -> Result<crate::models::lifecycle::LifecycleConfiguration, String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut state = LifecycleParseState::default();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                state.start(&name);
            }
            Ok(Event::Text(e)) => {
                let decoded = e.decode().unwrap_or_default();
                state.set_text(unescape(&decoded).unwrap_or_default().to_string());
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                state.end(&name);
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {e}")),
            _ => {}
        }
    }

    Ok(state.finish())
}

#[derive(Default)]
struct LifecycleParseState {
    rules: Vec<Rule>,
    current_rule: Option<Rule>,
    current_filter: Option<Filter>,
    current_expiration: Option<Expiration>,
    current_noncurrent_version_expiration: Option<NoncurrentVersionExpiration>,
    current_transition: Option<Transition>,
    current_tag: Option<(String, String)>,
    text: String,
}

impl LifecycleParseState {
    fn start(&mut self, name: &str) {
        match name {
            "Rule" => self.current_rule = Some(Self::new_rule()),
            "Filter" => {
                self.current_filter = Some(Filter {
                    prefix: None,
                    tags: Vec::new(),
                });
            }
            "Expiration" => self.current_expiration = Some(Self::new_expiration()),
            "NoncurrentVersionExpiration" => {
                self.current_noncurrent_version_expiration =
                    Some(NoncurrentVersionExpiration { noncurrent_days: 0 });
            }
            "Transition" => self.current_transition = Some(Self::new_transition()),
            "Tag" => self.current_tag = Some((String::new(), String::new())),
            _ => {}
        }
        self.text.clear();
    }

    fn set_text(&mut self, text: String) {
        self.text = text;
    }

    fn end(&mut self, name: &str) {
        self.end_rule_field(name);
        self.end_filter_field(name);
        self.end_action_field(name);
        self.close_element(name);
        self.text.clear();
    }

    fn finish(self) -> LifecycleConfiguration {
        LifecycleConfiguration { rules: self.rules }
    }

    fn new_rule() -> Rule {
        Rule {
            id: None,
            status: Status::Disabled,
            filter: None,
            expiration: None,
            noncurrent_version_expiration: None,
            transitions: Vec::new(),
        }
    }

    fn new_expiration() -> Expiration {
        Expiration {
            days: None,
            date: None,
            expired_object_delete_marker: None,
        }
    }

    fn new_transition() -> Transition {
        Transition {
            days: None,
            date: None,
            storage_class: StorageClass::Standard,
        }
    }

    fn end_rule_field(&mut self, name: &str) {
        if let Some(ref mut rule) = self.current_rule {
            match name {
                "ID" => rule.id = Some(self.text.clone()),
                "Status" => {
                    rule.status = if self.text == "Enabled" {
                        Status::Enabled
                    } else {
                        Status::Disabled
                    };
                }
                _ => {}
            }
        }
    }

    fn end_filter_field(&mut self, name: &str) {
        match name {
            "Prefix" => {
                if let Some(ref mut filter) = self.current_filter {
                    filter.prefix = Some(self.text.clone());
                }
            }
            "Key" => {
                if let Some(ref mut tag) = self.current_tag {
                    tag.0.clone_from(&self.text);
                }
            }
            "Value" => {
                if let Some(ref mut tag) = self.current_tag {
                    tag.1.clone_from(&self.text);
                }
            }
            "Tag" => self.finish_tag(),
            _ => {}
        }
    }

    fn end_action_field(&mut self, name: &str) {
        match name {
            "Days" => self.set_days(),
            "NoncurrentDays" => self.set_noncurrent_days(),
            "Date" => self.set_date(),
            "ExpiredObjectDeleteMarker" => self.set_expired_marker(),
            "StorageClass" => self.set_storage_class(),
            _ => {}
        }
    }

    fn close_element(&mut self, name: &str) {
        match name {
            "Filter" => {
                if let (Some(ref mut rule), Some(filter)) =
                    (&mut self.current_rule, self.current_filter.take())
                {
                    rule.filter = Some(filter);
                }
            }
            "Expiration" => {
                if let (Some(ref mut rule), Some(exp)) =
                    (&mut self.current_rule, self.current_expiration.take())
                {
                    rule.expiration = Some(exp);
                }
            }
            "NoncurrentVersionExpiration" => self.finish_noncurrent_expiration(),
            "Transition" => self.finish_transition(),
            "Rule" => {
                if let Some(rule) = self.current_rule.take() {
                    self.rules.push(rule);
                }
            }
            _ => {}
        }
    }

    fn finish_tag(&mut self) {
        if let (Some(ref mut filter), Some(tag)) =
            (&mut self.current_filter, self.current_tag.take())
        {
            filter.tags.push(Tag {
                key: tag.0,
                value: tag.1,
            });
        }
    }

    fn set_days(&mut self) {
        if let Ok(days) = self.text.parse::<u32>() {
            if let Some(ref mut exp) = self.current_expiration {
                exp.days = Some(days);
            } else if let Some(ref mut trans) = self.current_transition {
                trans.days = Some(days);
            }
        }
    }

    fn set_noncurrent_days(&mut self) {
        if let (Ok(days), Some(ref mut noncurrent_expiration)) = (
            self.text.parse::<u32>(),
            &mut self.current_noncurrent_version_expiration,
        ) {
            noncurrent_expiration.noncurrent_days = days;
        }
    }

    fn set_date(&mut self) {
        if let Some(ref mut exp) = self.current_expiration {
            exp.date = Some(self.text.clone());
        } else if let Some(ref mut trans) = self.current_transition {
            trans.date = Some(self.text.clone());
        }
    }

    fn set_expired_marker(&mut self) {
        if let Some(ref mut exp) = self.current_expiration {
            exp.expired_object_delete_marker = Some(self.text == "true");
        }
    }

    fn set_storage_class(&mut self) {
        if let Some(ref mut trans) = self.current_transition {
            trans.storage_class = match self.text.as_str() {
                "GLACIER" => StorageClass::Glacier,
                "DEEP_ARCHIVE" => StorageClass::DeepArchive,
                _ => StorageClass::Standard,
            };
        }
    }

    fn finish_noncurrent_expiration(&mut self) {
        if let (Some(ref mut rule), Some(noncurrent_expiration)) = (
            &mut self.current_rule,
            self.current_noncurrent_version_expiration.take(),
        ) {
            rule.noncurrent_version_expiration = Some(noncurrent_expiration);
        }
    }

    fn finish_transition(&mut self) {
        if let (Some(ref mut rule), Some(trans)) =
            (&mut self.current_rule, self.current_transition.take())
        {
            rule.transitions.push(trans);
        }
    }
}
