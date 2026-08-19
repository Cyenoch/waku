//! Re-export of the canonical provider endpoint types.

pub use wakuwaku_provider::{
    ApiFormat, AuthEndpoints, AuthMethod, AuthPhase, CatalogSource, ExternalProvider, LoginMethod,
    ModelCapabilities, ModelCatalog, ModelCatalogEntry, ProviderAuthStatus, ProviderId,
    ProviderLimits, ProviderPreset, ReasoningEffortOption, SecretString, ServiceTier,
    TransportProfile, UnsupportedReason, is_pinned_xai_token_endpoint, xai_oauth_seed,
};
