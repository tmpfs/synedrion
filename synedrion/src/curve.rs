//! This module is an adapter to the ECC backend.
//! `elliptic_curves` has a somewhat unstable API,
//! and we isolate all the related logic here.
// TODO (#27): make the library generic over the curve

mod arithmetic;
mod ecdsa;

pub(crate) use arithmetic::ORDER;

pub use self::ecdsa::RecoverableSignature;
pub use arithmetic::{Point, Scalar};
