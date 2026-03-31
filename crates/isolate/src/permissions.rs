//! Minimal permissions implementation for deno_fetch.
//!
//! We use `Permissions::allow_all()` so all network access is permitted without
//! prompting. The `PermissionDescriptorParser` trait is required by
//! `PermissionsContainer` but its methods are never called when all permissions
//! are granted (the check macros short-circuit on `is_allow_all()`).

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

/// Stub parser that panics if called — safe because `Permissions::allow_all()`
/// short-circuits all checks before reaching the parser.
#[derive(Debug)]
struct AllowAllDescriptorParser;

impl PermissionDescriptorParser for AllowAllDescriptorParser {
    fn parse_read_descriptor(
        &self,
        _text: &str,
    ) -> Result<ReadDescriptor, PathResolveError> {
        unreachable!("parser should not be called with allow_all permissions")
    }

    fn parse_write_descriptor(
        &self,
        _text: &str,
    ) -> Result<WriteDescriptor, PathResolveError> {
        unreachable!("parser should not be called with allow_all permissions")
    }

    fn parse_net_descriptor(
        &self,
        _text: &str,
    ) -> Result<NetDescriptor, NetDescriptorParseError> {
        unreachable!("parser should not be called with allow_all permissions")
    }

    fn parse_import_descriptor(
        &self,
        _text: &str,
    ) -> Result<ImportDescriptor, NetDescriptorParseError> {
        unreachable!("parser should not be called with allow_all permissions")
    }

    fn parse_env_descriptor(
        &self,
        _text: &str,
    ) -> Result<EnvDescriptor, EnvDescriptorParseError> {
        unreachable!("parser should not be called with allow_all permissions")
    }

    fn parse_sys_descriptor(
        &self,
        _text: &str,
    ) -> Result<SysDescriptor, SysDescriptorParseError> {
        unreachable!("parser should not be called with allow_all permissions")
    }

    fn parse_allow_run_descriptor(
        &self,
        _text: &str,
    ) -> Result<AllowRunDescriptorParseResult, RunDescriptorParseError> {
        unreachable!("parser should not be called with allow_all permissions")
    }

    fn parse_deny_run_descriptor(
        &self,
        _text: &str,
    ) -> Result<DenyRunDescriptor, PathResolveError> {
        unreachable!("parser should not be called with allow_all permissions")
    }

    fn parse_ffi_descriptor(
        &self,
        _text: &str,
    ) -> Result<FfiDescriptor, PathResolveError> {
        unreachable!("parser should not be called with allow_all permissions")
    }

    fn parse_path_query<'a>(
        &self,
        _path: Cow<'a, Path>,
    ) -> Result<PathQueryDescriptor<'a>, PathResolveError> {
        unreachable!("parser should not be called with allow_all permissions")
    }

    fn parse_special_file_descriptor<'a>(
        &self,
        _path: PathQueryDescriptor<'a>,
    ) -> Result<SpecialFilePathQueryDescriptor<'a>, PathResolveError> {
        unreachable!("parser should not be called with allow_all permissions")
    }

    fn parse_net_query(
        &self,
        _text: &str,
    ) -> Result<NetDescriptor, NetDescriptorParseError> {
        unreachable!("parser should not be called with allow_all permissions")
    }

    fn parse_run_query<'a>(
        &self,
        _requested: &'a str,
    ) -> Result<RunQueryDescriptor<'a>, RunDescriptorParseError> {
        unreachable!("parser should not be called with allow_all permissions")
    }
}

/// Create a `PermissionsContainer` that allows all operations.
pub fn appbase_permissions_container() -> PermissionsContainer {
    PermissionsContainer::allow_all(Arc::new(AllowAllDescriptorParser))
}
