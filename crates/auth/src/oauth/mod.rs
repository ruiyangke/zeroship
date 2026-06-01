//! OAuth flows the auth service drives itself (as opposed to the federation
//! provider clients in [`crate::identity::oauth`], which let users log in *with*
//! Google/GitHub).
//!
//! Today this hosts the headless authorization-code dance ([`headless`]) the
//! in-page `POST /password` login endpoint uses to mint a code for an
//! already-verified subject.

pub mod headless;
