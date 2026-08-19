//! Re-export of the canonical provider endpoint types.

pub use wakuwaku_provider::{
    ApiFormat, AuthEndpoints, AuthMethod, AuthPhase, CatalogSource, ExternalProvider, LoginMethod,
    MODELS_DEV_API_URL, ModelCapabilities, ModelCatalog, ModelCatalogEntry, ModelsDevCatalog,
    ProviderAuthStatus, ProviderId, ProviderLimits, ProviderPreset, ReasoningEffortOption,
    SecretString, ServiceTier, TransportProfile, UnsupportedReason, enrich_catalog_from_models_dev,
    is_pinned_xai_token_endpoint, models_dev_source_key, parse_models_dev_document, xai_oauth_seed,
};
