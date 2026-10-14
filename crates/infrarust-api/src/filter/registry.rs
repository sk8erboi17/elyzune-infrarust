//! Sealed registry traits for filter registration.
//!
//! Only the proxy core implements these traits. Plugins use them
//! via [`PluginContext::codec_filters()`] and [`PluginContext::transport_filters()`].

use crate::error::ErrorKind;

use super::codec::CodecFilterFactory;
use super::translate::ProtocolTranslatorFactory;
use super::transport::TransportFilter;

pub mod private {
    /// Sealed — only the proxy implements these registries.
    pub trait Sealed {}
}

pub const PROXY_FILTER_OWNER: &str = "infrarust";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FilterRegistryError {
    #[error("filter '{id}' is owned by '{owner}'")]
    OwnedBy { id: String, owner: String },
    #[error("no filter '{0}' is registered")]
    NotFound(String),
}

impl FilterRegistryError {
    /// The [`ErrorKind`] a WASM guest receives for this error.
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::OwnedBy { .. } => ErrorKind::Conflict,
            Self::NotFound(_) => ErrorKind::NotFound,
        }
    }
}

/// Registry for [`CodecFilterFactory`] instances.
///
/// Plugins register factories here; the proxy creates per-connection
/// instances from them when a new session is established.
pub trait CodecFilterRegistry: Send + Sync + private::Sealed {
    /// Registers a codec filter factory.
    fn register(&self, factory: Box<dyn CodecFilterFactory>) -> Result<(), FilterRegistryError>;

    /// Removes a codec filter factory by its metadata id.
    fn unregister(&self, filter_id: &str) -> Result<(), FilterRegistryError>;

    /// Installs the proxy's protocol translator factory.
    ///
    /// There is at most one: two translators stacked on the same backend
    /// socket would each believe it owns the version boundary. Registering
    /// while another plugin's factory is installed fails with
    /// [`FilterRegistryError::OwnedBy`]; the same plugin registering again
    /// replaces its own factory. Sessions already connected keep the
    /// translator they were created with.
    fn register_translator(
        &self,
        factory: Box<dyn ProtocolTranslatorFactory>,
    ) -> Result<(), FilterRegistryError>;

    /// Removes this plugin's protocol translator factory, by its id.
    fn unregister_translator(&self, translator_id: &str) -> Result<(), FilterRegistryError>;
}

/// Registry for [`TransportFilter`] instances.
///
/// Plugins register transport filters here; the proxy calls them
/// for every accepted TCP connection.
pub trait TransportFilterRegistry: Send + Sync + private::Sealed {
    /// Registers a transport filter.
    fn register(&self, filter: Box<dyn TransportFilter>) -> Result<(), FilterRegistryError>;

    /// Removes a transport filter by its metadata id.
    fn unregister(&self, filter_id: &str) -> Result<(), FilterRegistryError>;
}
