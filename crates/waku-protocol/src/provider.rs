//! Re-export of the canonical provider endpoint types.

pub use waku_provider::{
    ApiFormat, AuthEndpoints, AuthMethod, AuthPhase, CatalogSource, ExternalProvider, LoginMethod,
    ModelCapabilities, ModelCatalog, ModelCatalogEntry, ProviderAuthStatus, ProviderId,
    ProviderLimits, ProviderPreset, SecretString, ServiceTier, TransportProfile, UnsupportedReason,
    is_pinned_xai_token_endpoint, xai_oauth_seed,
};
