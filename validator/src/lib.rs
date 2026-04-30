//! Semantic validator for the containers-db tool catalog.
//!
//! Owns every cross-file rule that JSON Schema cannot express. ajv handles
//! shape; this crate handles meaning. See `rules` for the rule set.

pub mod catalog;
pub mod rules;
