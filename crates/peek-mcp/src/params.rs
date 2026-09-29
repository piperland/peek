//! Reading a tool's arguments, and refusing the ones that do not make sense.
//!
//! # The defect this exists to prevent
//!
//! Audit D section F, on the engine this project replaces:
//!
//! > **Silent parameter degradation on three paths:** `--direction sideways` → `Outbound`;
//! > `--kind nonsense` → no filter; `find-symbol` with no args → 50 arbitrary symbols.
//!
//! All three are silent parameter degradation, and all three return a confident answer to a request
//! the caller did not make. A model makes the same mistakes as a person, and is worse at noticing
//! them, because it has no reason to doubt a well-formed response.
//!
//! So the rules here are:
//!
//! * **An unknown argument is an error**, not a shrug. `callers` given a `depth` says that
//!   `callers` is one hop by definition and names `dependents` as the tool that takes a depth.
//! * **A parameter of the wrong type is an error** that names the type it found.
//! * **A missing required parameter is an error** that names it and says what it is for.
//! * **A string that does not name a relation kind is an error** that lists the kinds, because
//!   "no filter" is what a typo silently becomes.
//!
//! # Why it is hand-written rather than `serde` derives
//!
//! Because `serde_json::Value` has already thrown away the schema by the time a derived struct
//! sees it, and a derived struct's error for a bad argument is a line of `serde` internal paths
//! (`arguments.target: string at line 1 column 24`) that a model has to decode. A hand-written
//! reader produces a sentence. The cost is that the schema in [`crate::tool`] and the reader here
//! are two descriptions of the same thing, so [`Args::check_schema`] exists to keep them in step —
//! the catalogue is checked against the reader at test time rather than trusted.

use serde_json::{Map, Value};

use crate::outcome::ToolError;

/// The reader for one call's `arguments` object.
pub struct Args<'a> {
    tool: &'static str,
    /// The call's arguments, or `None` when the call sent none.
    object: Option<&'a Map<String, Value>>,
    /// What to say about an argument this tool does not have, keyed by that argument's name.
    ///
    /// The table is the interesting part. A model that reached for the wrong tool has usually
    /// reached for an argument that *would* be right elsewhere, so the refusal names the right
    /// place to put it rather than only saying no.
    hints: &'a [(&'static str, &'static str)],
}

impl<'a> Args<'a> {
    /// Read the `arguments` of a `tools/call` request.
    ///
    /// `arguments` is optional in the protocol and defaults to no arguments, which is a valid call
    /// for a tool that takes none and a missing-argument error for a tool that does. A non-object
    /// is refused with the type it actually had: an array of arguments is a common client mistake
    /// and "expected an object, got an array" is a sentence that fixes it.
    pub fn new(
        tool: &'static str,
        arguments: Option<&'a Value>,
        hints: &'a [(&'static str, &'static str)],
    ) -> Result<Self, ToolError> {
        match arguments {
            None | Some(Value::Null) => Ok(Self {
                tool,
                object: None,
                hints,
            }),
            Some(Value::Object(object)) => Ok(Self {
                tool,
                object: Some(object),
                hints,
            }),
            Some(other) => Err(ToolError::argument(
                format!(
                    "`{tool}` takes an object of named arguments; this call sent {}",
                    type_name(other)
                ),
                "every argument is named, and every name is listed in the tool's input schema",
            )),
        }
    }

    /// One argument, or `None` for one the call did not send or sent as `null`.
    ///
    /// An explicit `null` is treated as absent, which is what a client that always sends every key
    /// in its template means by it.
    fn get(&self, name: &str) -> Option<&'a Value> {
        self.object?.get(name).filter(|value| !value.is_null())
    }

    /// Every key the call sent, whether or not it means anything to this tool.
    fn keys(&self) -> impl Iterator<Item = &String> + '_ {
        self.object.into_iter().flat_map(|object| object.keys())
    }

    /// A required string.
    pub fn required_string(
        &self,
        name: &'static str,
        what: &str,
    ) -> Result<String, ToolError> {
        match self.get(name) {
            None => Err(self.missing(name, what)),
            Some(Value::String(text)) => Ok(text.clone()),
            Some(other) => Err(self.wrong_type(name, "a string", other)),
        }
    }

    /// An optional string. An explicit `null` is the same as absent, which is what a client that
    /// always sends every key means by it.
    pub fn optional_string(&self, name: &'static str) -> Result<Option<String>, ToolError> {
        match self.get(name) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(other) => Err(self.wrong_type(name, "a string", other)),
        }
    }

    /// A required non-negative integer.
    pub fn required_u64(
        &self,
        name: &'static str,
        what: &str,
    ) -> Result<u64, ToolError> {
        match self.get(name) {
            None => Err(self.missing(name, what)),
            Some(value) => self.as_u64(name, value),
        }
    }

    /// An optional non-negative integer.
    pub fn optional_u64(&self, name: &'static str) -> Result<Option<u64>, ToolError> {
        match self.get(name) {
            None => Ok(None),
            Some(value) => self.as_u64(name, value).map(Some),
        }
    }

    /// An optional integer that has to fit a `u32`, refused with the number it was rather than by
    /// saturating it. A saturated `depth` is a walk of a different graph than the one asked for,
    /// which is precisely the silent-wrong-answer this module exists to stop.
    pub fn optional_u32(&self, name: &'static str) -> Result<Option<u32>, ToolError> {
        match self.optional_u64(name)? {
            None => Ok(None),
            Some(value) => u32::try_from(value).map(Some).map_err(|_| {
                ToolError::argument(
                    format!("`{}`: `{name}` is {value}, which is larger than the largest depth", self.tool),
                    "a depth is a number of hops; the engine bounds the work with `max_visited` and \
                     says so in the answer, so there is no reason to ask for more than this",
                )
            }),
        }
    }

    /// An optional boolean.
    pub fn optional_bool(&self, name: &'static str) -> Result<Option<bool>, ToolError> {
        match self.get(name) {
            None => Ok(None),
            Some(Value::Bool(flag)) => Ok(Some(*flag)),
            Some(other) => Err(self.wrong_type(name, "true or false", other)),
        }
    }

    /// An optional list of strings.
    pub fn optional_string_array(
        &self,
        name: &'static str,
    ) -> Result<Option<Vec<String>>, ToolError> {
        match self.get(name) {
            None => Ok(None),
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Value::String(text) => out.push(text.clone()),
                        other => {
                            return Err(ToolError::argument(
                                format!(
                                    "`{}`: `{name}` must be a list of strings, and it contains {}",
                                    self.tool,
                                    type_name(other)
                                ),
                                "paths are repository-relative and `/`-separated, for example \
                                 `src/payments/service.rs`",
                            ));
                        }
                    }
                }
                Ok(Some(out))
            }
            Some(other) => Err(self.wrong_type(name, "a list of strings", other)),
        }
    }

    /// Refuse every argument this tool does not have.
    ///
    /// Called last, by every handler, so that a call carrying one good argument and one
    /// meaningless one is refused rather than half-honoured. Half-honouring is the defect: the
    /// caller believes the meaningless argument did something.
    pub fn finish(&self, accepted: &[&'static str]) -> Result<(), ToolError> {
        for name in self.keys() {
            if accepted.contains(&name.as_str()) {
                continue;
            }
            let advice = self
                .hints
                .iter()
                .find(|(hinted, _)| *hinted == name.as_str())
                .map_or_else(
                    || {
                        format!(
                            "`{}` takes {}, and nothing else; send no arguments at all if you meant \
                             to change nothing",
                            self.tool,
                            list(accepted)
                        )
                    },
                    |(_, advice)| (*advice).to_owned(),
                );
            return Err(ToolError::argument(
                format!("`{}` has no argument named `{name}`", self.tool),
                advice,
            ));
        }
        Ok(())
    }

    /// Reject a `kind` string that is not a relation kind the engine knows.
    ///
    /// The alternative is the audit's `--kind nonsense` → no filter: a caller who misspells a kind
    /// gets every kind back and reads the result as a filtered one.
    pub fn relation_kind(
        &self,
        name: &'static str,
    ) -> Result<Option<peek_core::model::RelationKind>, ToolError> {
        let Some(text) = self.optional_string(name)? else {
            return Ok(None);
        };
        peek_core::model::RelationKind::parse(&text).map(Some).ok_or_else(|| {
            ToolError::argument(
                format!("`{text}` is not a relation kind this build has"),
                format!(
                    "the kinds are: {}",
                    peek_core::model::relation_kind_names().join(", ")
                ),
            )
        })
    }

    fn as_u64(&self, name: &'static str, value: &Value) -> Result<u64, ToolError> {
        match value {
            Value::Number(number) => number.as_u64().ok_or_else(|| {
                ToolError::argument(
                    format!(
                        "`{}`: `{name}` is {number}, which is not a whole number this engine can \
                         use",
                        self.tool
                    ),
                    "token counts and depths are whole numbers; a negative or fractional value has \
                     no meaning here",
                )
            }),
            other => Err(self.wrong_type(name, "a whole number", other)),
        }
    }

    fn missing(&self, name: &'static str, what: &str) -> ToolError {
        ToolError::argument(
            format!("`{}` needs a `{name}` argument", self.tool),
            format!("`{name}` is {what}"),
        )
    }

    fn wrong_type(&self, name: &'static str, expected: &str, found: &Value) -> ToolError {
        ToolError::argument(
            format!(
                "`{}`: `{name}` must be {expected}, and this call sent {}",
                self.tool,
                type_name(found)
            ),
            format!("the input schema for `{name}` is on this tool's description"),
        )
    }
}

/// What a caller sent, in words.
///
/// Named rather than inlined so a refusal reads the same whichever way the value arrived, and so
/// the list is one place to extend.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// A list of names, for a sentence.
fn list(names: &[&str]) -> String {
    match names {
        [] => "no arguments".to_owned(),
        [one] => format!("`{one}`"),
        [one, two] => format!("`{one}` and `{two}`"),
        many => {
            let head = many[..many.len() - 1]
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{head} and `{}`", many[many.len() - 1])
        }
    }
}
