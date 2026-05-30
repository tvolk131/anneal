//! Structured load errors that do **not** leak Starlark types.
//!
//! A [`LoadError`] carries a human message, an optional `file:line:col` location,
//! and a full compiler-style rendering for display. Parse/evaluation errors get
//! their location from Starlark's spans (Spike A); schema errors get it from the
//! recorded call-site location of the offending target.

use std::fmt;

/// A failure loading a `BUILD` file.
#[derive(Debug, Clone)]
pub struct LoadError {
    message: String,
    location: Option<String>,
    rendered: String,
}

impl LoadError {
    /// The concise error message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The `file:line:col` location, if known.
    pub fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }

    /// Build from a Starlark error, extracting span + rendered diagnostic while
    /// discarding the Starlark type itself.
    pub(crate) fn from_starlark(err: starlark::Error) -> Self {
        let rendered = format!("{err}");
        let location = err.span().map(|span| span.to_string());
        // The rendered form is compiler-style ("error: <msg>"); pull the message out.
        let message = rendered
            .lines()
            .find_map(|l| l.trim().strip_prefix("error: "))
            .unwrap_or(&rendered)
            .to_owned();
        LoadError {
            message,
            location,
            rendered,
        }
    }

    /// A schema/validation error attributed to a target at `location`.
    pub(crate) fn schema(message: impl Into<String>, location: Option<String>) -> Self {
        let message = message.into();
        let rendered = match &location {
            Some(loc) => format!("error: {message}\n  --> {loc}"),
            None => format!("error: {message}"),
        };
        LoadError {
            message,
            location,
            rendered,
        }
    }

    /// An I/O error (e.g. the `BUILD` file could not be read).
    pub(crate) fn io(message: impl Into<String>) -> Self {
        let message = message.into();
        let rendered = format!("error: {message}");
        LoadError {
            message,
            location: None,
            rendered,
        }
    }
}

impl From<starlark::Error> for LoadError {
    fn from(err: starlark::Error) -> Self {
        LoadError::from_starlark(err)
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.rendered)
    }
}

impl std::error::Error for LoadError {}
