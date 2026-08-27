//! Everything flock says to GitHub over GraphQL.
//!
//! [`graphql`] owns the transport — one `curl` subprocess, the token on stdin,
//! and a closed error enum — and is the only module that talks to the endpoint.
//! [`repos`] and [`issues`] build documents and parse responses; both are pure
//! apart from the single `execute` call each fetch makes, which is what keeps
//! query shape and response handling testable without a network.

pub(crate) mod drop;
pub(crate) mod graphql;
pub(crate) mod issues;
pub(crate) mod repos;
