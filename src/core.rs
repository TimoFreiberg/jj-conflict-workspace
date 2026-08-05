//! Pure algorithm boundaries. Implementations are intentionally deferred.

use crate::domain::{ApplyPlan, ApplyValidationRequest, ParsedDocument};
use crate::error::DomainError;

/// Parse the supported snapshot conflict grammar from source bytes.
///
/// Task 02 will implement marker and section parsing. This foundation keeps
/// the signature byte-only and does not perform lossy UTF-8 conversion.
pub fn parse_snapshot(_input: &[u8]) -> Result<ParsedDocument, DomainError> {
    Err(DomainError::NotImplemented {
        operation: "parse_snapshot",
    })
}

/// Materialize a scaffold by copying outside bytes and replacing each region
/// with its first logical term. The first term is only a structural seed; it
/// is not a semantic merge choice. Task 02 will implement this operation.
pub fn materialize_scaffold(_document: &ParsedDocument) -> Result<Vec<u8>, DomainError> {
    Err(DomainError::NotImplemented {
        operation: "materialize_scaffold",
    })
}

/// Validate a resolved file and produce an original-coordinate apply plan.
/// Byte comparison and diff policy belong to later tasks.
pub fn validate_apply(_request: ApplyValidationRequest<'_>) -> Result<ApplyPlan, DomainError> {
    Err(DomainError::NotImplemented {
        operation: "validate_apply",
    })
}
