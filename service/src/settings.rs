//! The switches — the names, and what each one turns off.
//!
//! **A switch is tri-state per site**: absent (the instance's default), on,
//! off. The instance's default comes from the environment ([`crate::config`]);
//! the site's own setting overrides it.
//!
//! **Why one column and not a column per switch**: the set grows, and a new
//! switch should not cost a schema version. The names live in one list
//! ([`SWITCHES`]) ⇒ ⇒ the API can refuse a name that is not ours, and the
//! documentation can be written from the same list.
//!
//! **What a switch is for**: some of them are about privacy (the raw address,
//! the page title), some are about space (the coordinates, the web vitals,
//! the advertising click ids). The two are kept apart on purpose: the text
//! columns cost **nothing** (the dictionary eats them) ⇒ ⇒ a text column is
//! never turned off to save space, only to keep a person's data out.

use serde_json::Value;

/// What a switch's value looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// **A flag**: on or off — the site says nothing, on, or off.
    Flag,
    /// **A word**: the site's own string, which means something by itself.
    Word,
}

/// One switch.
#[derive(Debug, Clone, Copy)]
pub struct Switch {
    /// The name — the key in the site's settings.
    pub name: &'static str,
    /// What it turns off.
    pub what: &'static str,
    /// What its value looks like.
    pub kind: Kind,
}

/// The switches a site may set.
pub const SWITCHES: &[Switch] = &[
    Switch {
        name: "store_raw",
        what: "the raw address and the raw agent",
        kind: Kind::Flag,
    },
    Switch {
        name: "store_titles",
        what: "the page title",
        kind: Kind::Flag,
    },
    Switch {
        name: "store_referrer_path",
        what: "the referrer's path",
        kind: Kind::Flag,
    },
    Switch {
        name: "store_referrer_query",
        what: "the referrer's query",
        kind: Kind::Flag,
    },
    Switch {
        name: "keep_query",
        what: "the query string",
        kind: Kind::Flag,
    },
    Switch {
        name: "store_click_ids",
        what: "the advertising click ids' values",
        kind: Kind::Flag,
    },
    Switch {
        name: "store_coordinates",
        what: "the coordinates",
        kind: Kind::Flag,
    },
    Switch {
        name: "store_web_vitals",
        what: "the web vitals",
        kind: Kind::Flag,
    },
    Switch {
        name: "store_session_start",
        what: "the session's start on every row",
        kind: Kind::Flag,
    },
    Switch {
        name: "store_language",
        what: "the visitor's language",
        kind: Kind::Flag,
    },
    Switch {
        name: "store_screen",
        what: "the screen's resolution",
        kind: Kind::Flag,
    },
    Switch {
        name: "session_scope",
        what: "the scope the visitor id is derived in: site (the default) · instance · a word of your own",
        kind: Kind::Word,
    },
];

/// The setting of one switch in a site's settings.
///
/// **`None` means the site says nothing** ⇒ ⇒ the instance's default stands.
pub fn of(settings: &Value, name: &str) -> Option<bool> {
    settings.get(name).and_then(|value| value.as_bool())
}

/// The setting of one **word** switch in a site's settings.
///
/// **`None` means the site says nothing** ⇒ ⇒ the instance's default stands.
pub fn word(settings: &Value, name: &str) -> Option<String> {
    settings.get(name).and_then(|value| value.as_str()).map(String::from)
}

/// Check the settings: an object, and every key one of ours.
///
/// **A `null` is allowed** ⇒ ⇒ it means "inherit", the same as an absent key.
pub fn validate(settings: &Value) -> Result<(), String> {
    let Some(object) = settings.as_object() else {
        return Err("the settings must be an object".to_string());
    };
    for (name, value) in object {
        let Some(switch) = SWITCHES.iter().find(|switch| switch.name == name) else {
            return Err(format!("unknown switch: {name}"));
        };
        if value.is_null() {
            continue;
        }
        match switch.kind {
            Kind::Flag if value.is_boolean() => {}
            Kind::Flag => return Err(format!("the switch {name} must be true, false or null")),
            Kind::Word if value.is_string() => {}
            Kind::Word => return Err(format!("the switch {name} must be a word or null")),
        }
    }
    Ok(())
}

/// The settings of a site, as they came out of the database.
///
/// **A site that was never set has nothing** ⇒ ⇒ the empty object ⇒ ⇒ every
/// switch falls back to the instance's default.
pub fn parse(text: Option<&str>) -> Value {
    match text {
        Some(text) if !text.trim().is_empty() => serde_json::from_str(text)
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
        _ => Value::Object(serde_json::Map::new()),
    }
}

/// The settings as they go into the database.
///
/// **An empty object is stored as nothing** ⇒ ⇒ a site that says nothing has
/// nothing in the column ⇒ ⇒ and the instance's default stands.
pub fn to_text(settings: &Value) -> Option<String> {
    match settings.as_object() {
        Some(object) if !object.is_empty() => {
            Some(Value::Object(object.clone()).to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_site_that_says_nothing_inherits_everything() {
        let settings = parse(None);
        assert_eq!(of(&settings, "store_raw"), None);
        assert_eq!(of(&settings, "store_click_ids"), None);
    }

    #[test]
    fn an_unknown_name_is_refused() {
        let settings = json!({"store_everything": true});
        assert!(validate(&settings).is_err());
    }

    #[test]
    fn a_name_that_is_not_a_switch_is_refused() {
        let settings = json!({"store_raw": "yes"});
        assert!(validate(&settings).is_err());
    }

    #[test]
    fn a_null_means_inherit() {
        let settings = json!({"store_raw": null});
        assert!(validate(&settings).is_ok());
        assert_eq!(of(&settings, "store_raw"), None);
    }

    #[test]
    fn an_empty_object_is_stored_as_nothing() {
        let settings = json!({});
        assert_eq!(to_text(&settings), None);
        let settings = json!({"store_raw": false});
        assert_eq!(
            to_text(&settings),
            Some("{\"store_raw\":false}".to_string())
        );
    }
}
