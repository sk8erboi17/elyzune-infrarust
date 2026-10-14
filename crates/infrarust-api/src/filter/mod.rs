//! Filter system for packet and transport-level interception.
//!
//! Two filter levels are provided:
//!
//! - **Codec filters** ([`CodecFilterFactory`] / [`CodecFilterInstance`]):
//!   Operate on framed Minecraft packets. Synchronous, per-connection instances.
//!   Run on the hot path — must be fast (< 1 us).
//!
//! - **Transport filters** ([`TransportFilter`]):
//!   Gate each accepted TCP connection before Minecraft framing. Async, shared
//!   instances. Can reject connections at the TCP level.
//!
//! - **Protocol translators** ([`ProtocolTranslatorFactory`] /
//!   [`ProtocolTranslator`]): own the backend socket of an intercepted
//!   session and translate between the client's protocol version and the
//!   backend's, in every state and both directions. Registered through the
//!   codec filter registry, so they need the same capability.

pub mod codec;
pub mod metadata;
pub mod registry;
pub mod translate;
pub mod transport;

pub use codec::{
    CodecFilterError, CodecFilterFactory, CodecFilterInstance, CodecSessionInit, CodecVerdict,
    ConnectionSide, FrameOutput,
};
pub use metadata::{FilterMetadata, FilterPriority};
pub use registry::{
    CodecFilterRegistry, FilterRegistryError, PROXY_FILTER_OWNER, TransportFilterRegistry,
};
pub use translate::{
    ProtocolTranslator, ProtocolTranslatorFactory, TranslatorInit, TranslatorOutput,
};
pub use transport::{FilterVerdict, TransportContext, TransportFilter};
