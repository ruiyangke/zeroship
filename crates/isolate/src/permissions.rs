//! Minimal permissions implementation for deno_fetch.
//!
//! We use `Permissions::allow_all()` so all network access is permitted without
//! prompting. The `PermissionDescriptorParser` trait is required by
//! `PermissionsContainer` but its methods are never called when all permissions
//! are granted (the check macros short-circuit on `is_allow_all()`).
//!
//! All parser methods return errors instead of panicking, so that a future deno
//! version change that alters the short-circuit path will fail safely rather
//! than crashing the server.

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use deno_permissions::{
    AllowRunDescriptorParseResult, DenyRunDescriptor, EnvDescriptor,
    EnvDescriptorParseError, FfiDescriptor, ImportDescriptor, NetDescriptor,
    NetDescriptorParseError, PathQueryDescriptor, PathResolveError,
    PermissionDescriptorParser, PermissionsContainer, ReadDescriptor,
    RunDescriptorParseError, RunQueryDescriptor, SpecialFilePathQueryDescriptor,
    SysDescriptor, SysDescriptorParseError, WriteDescriptor,
};

/// Permission denied IO error for use in parser stubs.
fn denied_io_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "operation not permitted in appbase isolates",
    )
}

/// Stub parser whose methods return errors instead of panicking.
///
/// With `Permissions::allow_all()` these methods are never called (the
/// permission check short-circuits). If a future deno update changes that
/// behaviour, the server will surface a permission error instead of crashing.
#[derive(Debug)]
struct AllowAllDescriptorParser;

impl PermissionDescriptorParser for AllowAllDescriptorParser {
    fn parse_read_descriptor(
        &self,
        _text: &str,
    ) -> Result<ReadDescriptor, PathResolveError> {
        Err(PathResolveError::CwdResolve(denied_io_error()))
    }

    fn parse_write_descriptor(
        &self,
        _text: &str,
    ) -> Result<WriteDescriptor, PathResolveError> {
        Err(PathResolveError::CwdResolve(denied_io_error()))
    }

    fn parse_net_descriptor(
        &self,
        text: &str,
    ) -> Result<NetDescriptor, NetDescriptorParseError> {
        Err(NetDescriptorParseError::InvalidHost(text.to_string()))
    }

    fn parse_import_descriptor(
        &self,
        text: &str,
    ) -> Result<ImportDescriptor, NetDescriptorParseError> {
        Err(NetDescriptorParseError::InvalidHost(text.to_string()))
    }

    fn parse_env_descriptor(
        &self,
        _text: &str,
    ) -> Result<EnvDescriptor, EnvDescriptorParseError> {
        Err(EnvDescriptorParseError)
    }

    fn parse_sys_descriptor(
        &self,
        _text: &str,
    ) -> Result<SysDescriptor, SysDescriptorParseError> {
        Err(SysDescriptorParseError::Empty)
    }

    fn parse_allow_run_descriptor(
        &self,
        _text: &str,
    ) -> Result<AllowRunDescriptorParseResult, RunDescriptorParseError> {
        Err(RunDescriptorParseError::EmptyRunQuery)
    }

    fn parse_deny_run_descriptor(
        &self,
        _text: &str,
    ) -> Result<DenyRunDescriptor, PathResolveError> {
        Err(PathResolveError::CwdResolve(denied_io_error()))
    }

    fn parse_ffi_descriptor(
        &self,
        _text: &str,
    ) -> Result<FfiDescriptor, PathResolveError> {
        Err(PathResolveError::CwdResolve(denied_io_error()))
    }

    fn parse_path_query<'a>(
        &self,
        _path: Cow<'a, Path>,
    ) -> Result<PathQueryDescriptor<'a>, PathResolveError> {
        Err(PathResolveError::CwdResolve(denied_io_error()))
    }

    fn parse_special_file_descriptor<'a>(
        &self,
        _path: PathQueryDescriptor<'a>,
    ) -> Result<SpecialFilePathQueryDescriptor<'a>, PathResolveError> {
        Err(PathResolveError::CwdResolve(denied_io_error()))
    }

    fn parse_net_query(
        &self,
        text: &str,
    ) -> Result<NetDescriptor, NetDescriptorParseError> {
        Err(NetDescriptorParseError::InvalidHost(text.to_string()))
    }

    fn parse_run_query<'a>(
        &self,
        _requested: &'a str,
    ) -> Result<RunQueryDescriptor<'a>, RunDescriptorParseError> {
        Err(RunDescriptorParseError::EmptyRunQuery)
    }
}

/// Create a `PermissionsContainer` that allows all operations.
///
/// TODO: Restrict to network-only permissions for multi-tenant safety.
/// Currently uses `allow_all` which grants FS/FFI/run/env access if the
/// parser short-circuit is ever bypassed.
pub fn appbase_permissions_container() -> PermissionsContainer {
    PermissionsContainer::allow_all(Arc::new(AllowAllDescriptorParser))
}
