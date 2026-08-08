//! Router — candidate resolution (pure).
//!
//! This foundation lane re-exports the shared types so the Router lane and the
//! shell reference one canonical definition. The pure `resolve` function is
//! implemented by the Router lane (T2.x) via TDD.
//!
//! ## Contract
//! `resolve(&ConfigData, &impl BreakerView, &Tenant, model_key) ->
//! Result<Vec<Candidate>, RouteError>`
//!
//! ## Purity / time-injection
//! Pure and deterministic: no I/O, no global state, no time. Inputs fully
//! describe the output. Order of returned candidates is finalised by SWRR
//! afterwards; `resolve` only computes & filters the set (TenantModel gate →
//! model-providers ∩ tenant-providers → filter dead/soft-disabled/keyless).

pub use crate::model::{Candidate, RouteError};
