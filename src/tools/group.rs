//! This module is an adapter to the ECC backend.
//! `elliptic_curves` has a somewhat unstable API,
//! and we isolate all the related logic here.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::default::Default;
use core::ops::{Add, Mul, Sub};

use k256::elliptic_curve::group::ff::PrimeField;
use k256::elliptic_curve::{
    bigint::U256, // Note that this type is different from typenum::U256
    generic_array::typenum::marker_traits::Unsigned,
    generic_array::GenericArray,
    hash2curve::{ExpandMsgXmd, GroupDigest},
    ops::Reduce,
    point::AffineCoordinates,
    scalar::IsHigh,
    sec1::{EncodedPoint, FromEncodedPoint, ModulusSize, ToEncodedPoint},
    subtle::CtOption,
    Field,
    FieldBytesSize,
};
use k256::{ecdsa::hazmat::VerifyPrimitive, Secp256k1};
use rand_core::{CryptoRng, RngCore};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{digest::Digest, Sha256};

use crate::tools::hashing::{Chain, Hashable};
use crate::tools::serde::{deserialize, serialize, TryFromBytes};

pub(crate) type BackendScalar = k256::Scalar;
pub(crate) type BackendPoint = k256::ProjectivePoint;
pub(crate) type CompressedPointSize =
    <FieldBytesSize<Secp256k1> as ModulusSize>::CompressedPointSize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Scalar(BackendScalar);

impl Scalar {
    pub const ZERO: Self = Self(BackendScalar::ZERO);
    pub const ONE: Self = Self(BackendScalar::ONE);

    pub fn random(rng: &mut (impl CryptoRng + RngCore)) -> Self {
        Self(BackendScalar::random(rng))
    }

    pub fn random_in_range_j(rng: &mut (impl CryptoRng + RngCore)) -> Self {
        // TODO: find out what the range `\mathcal{J}` is.
        Self(BackendScalar::random(rng))
    }

    pub fn mul_by_generator(&self) -> Point {
        &Point::GENERATOR * self
    }

    pub fn pow(&self, exp: usize) -> Self {
        let mut result = Self::ONE;
        for _ in 0..exp {
            result = &result * self;
        }
        result
    }

    pub fn invert(&self) -> CtOption<Self> {
        self.0.invert().map(Self)
    }

    pub fn normalize(&self) -> Self {
        if self.0.is_high().into() {
            -self
        } else {
            *self
        }
    }

    pub fn from_digest(d: impl Digest<OutputSize = FieldBytesSize<k256::Secp256k1>>) -> Self {
        // There's currently no way to make the required digest output size
        // depend on the target scalar size, so we are hardcoding it to 256 bit
        // (that is, equal to the scalar size).
        Self(<BackendScalar as Reduce<U256>>::reduce_bytes(&d.finalize()))
    }

    pub fn to_be_bytes(self) -> k256::FieldBytes {
        // TODO: add a test that it really is a big endian representation - docs don't guarantee it.
        self.0.to_bytes()
    }

    pub fn repr_len() -> usize {
        <FieldBytesSize<Secp256k1> as Unsigned>::to_usize()
    }

    pub(crate) fn try_from_be_bytes(bytes: &[u8]) -> Result<Self, String> {
        let arr =
            GenericArray::<u8, FieldBytesSize<Secp256k1>>::from_exact_iter(bytes.iter().cloned())
                .ok_or("Invalid length of a curve scalar")?;

        BackendScalar::from_repr_vartime(arr)
            .map(Self)
            .ok_or_else(|| "Invalid curve scalar representation".into())
    }
}

impl Serialize for Scalar {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serialize(&self.0.to_bytes(), serializer)
    }
}

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize(deserializer)
    }
}

impl TryFromBytes for Scalar {
    type Error = String;

    fn try_from_bytes(bytes: &[u8]) -> Result<Self, Self::Error> {
        Self::try_from_be_bytes(bytes)
    }
}

pub(crate) fn zero_sum_scalars(rng: &mut (impl CryptoRng + RngCore), size: usize) -> Vec<Scalar> {
    // CHECK: do they all have to be non-zero?

    debug_assert!(size > 1);

    let mut scalars = (0..(size - 1))
        .map(|_| Scalar::random(rng))
        .collect::<Vec<_>>();
    let sum: Scalar = scalars
        .iter()
        .cloned()
        .reduce(|s1, s2| s1 + s2)
        .unwrap_or(Scalar::ZERO);
    scalars.push(-sum);
    scalars
}

#[derive(Clone, Debug)]
pub struct Signature(k256::ecdsa::Signature);

impl Signature {
    pub fn from_scalars(r: &Scalar, s: &Scalar) -> Option<Self> {
        // TODO: call `normalize_s()` on the result?
        // TODO: pass a message too and derive the recovery byte?
        k256::ecdsa::Signature::from_scalars(r.0, s.0)
            .map(Self)
            .ok()
    }

    pub fn verify(&self, vkey: &Point, message: &Scalar) -> bool {
        let verifier = vkey.0.to_affine();
        verifier
            .verify_prehashed(&message.0.to_bytes(), &self.0)
            .is_ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point(BackendPoint);

impl Point {
    pub const GENERATOR: Self = Self(BackendPoint::GENERATOR);

    pub const IDENTITY: Self = Self(BackendPoint::IDENTITY);

    // TODO: technically it can be any hash function from Point to Scalar, right?
    // so we can just rename it to `to_scalar()` or something.
    pub fn x_coordinate(&self) -> Scalar {
        let bytes = self.0.to_affine().x();
        Scalar(<BackendScalar as Reduce<U256>>::reduce_bytes(&bytes))
    }

    /// Hashes arbitrary data with the given domain separation tag
    /// into a valid EC point of the specified curve, using the algorithm described in the
    /// [IETF hash-to-curve standard](https://datatracker.ietf.org/doc/draft-irtf-cfrg-hash-to-curve/)
    pub fn from_data(dst: &[u8], data: &[&[u8]]) -> Option<Self> {
        Some(Self(
            k256::Secp256k1::hash_from_bytes::<ExpandMsgXmd<Sha256>>(data, &[dst]).ok()?,
        ))
    }

    pub(crate) fn try_from_compressed_bytes(bytes: &[u8]) -> Result<Self, String> {
        let ep = EncodedPoint::<Secp256k1>::from_bytes(bytes).map_err(|err| format!("{err}"))?;

        // Unwrap CtOption into Option
        let cp_opt: Option<BackendPoint> = BackendPoint::from_encoded_point(&ep).into();
        cp_opt
            .map(Self)
            .ok_or_else(|| "Invalid curve point representation".into())
    }

    pub(crate) fn to_compressed_array(self) -> GenericArray<u8, CompressedPointSize> {
        *GenericArray::<u8, CompressedPointSize>::from_slice(
            self.0.to_affine().to_encoded_point(true).as_bytes(),
        )
    }
}

impl Serialize for Point {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serialize(&self.to_compressed_array(), serializer)
    }
}

impl<'de> Deserialize<'de> for Point {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize(deserializer)
    }
}

impl TryFromBytes for Point {
    type Error = String;

    fn try_from_bytes(bytes: &[u8]) -> Result<Self, Self::Error> {
        Self::try_from_compressed_bytes(bytes)
    }
}

impl Hashable for Point {
    fn chain<C: Chain>(&self, digest: C) -> C {
        let arr = self.to_compressed_array();
        let arr_ref: &[u8] = arr.as_ref();
        digest.chain(&arr_ref)
    }
}

impl Default for Point {
    fn default() -> Self {
        Point::IDENTITY
    }
}

impl From<usize> for Scalar {
    fn from(val: usize) -> Self {
        // TODO: add a check that usize <= u64?
        Self(BackendScalar::from(val as u64))
    }
}

impl core::ops::Neg for Scalar {
    type Output = Self;
    fn neg(self) -> Self::Output {
        Self(-self.0)
    }
}

impl<'a> core::ops::Neg for &'a Scalar {
    type Output = Scalar;
    fn neg(self) -> Self::Output {
        Scalar(-self.0)
    }
}

impl Add<Scalar> for Scalar {
    type Output = Scalar;

    fn add(self, other: Scalar) -> Scalar {
        Scalar(self.0.add(other.0))
    }
}

impl Add<&Scalar> for &Scalar {
    type Output = Scalar;

    fn add(self, other: &Scalar) -> Scalar {
        Scalar(self.0.add(&(other.0)))
    }
}

impl Add<Point> for Point {
    type Output = Point;

    fn add(self, other: Point) -> Point {
        Point(self.0.add(other.0))
    }
}

impl Add<&Point> for &Point {
    type Output = Point;

    fn add(self, other: &Point) -> Point {
        Point(self.0.add(&(other.0)))
    }
}

impl Sub<&Scalar> for &Scalar {
    type Output = Scalar;

    fn sub(self, other: &Scalar) -> Scalar {
        Scalar(self.0.sub(&(other.0)))
    }
}

impl Mul<&Scalar> for &Point {
    type Output = Point;

    fn mul(self, other: &Scalar) -> Point {
        Point(self.0.mul(&(other.0)))
    }
}

impl Mul<&Scalar> for &Scalar {
    type Output = Scalar;

    fn mul(self, other: &Scalar) -> Scalar {
        Scalar(self.0.mul(&(other.0)))
    }
}

impl core::iter::Sum for Scalar {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.reduce(core::ops::Add::add).unwrap_or(Self::ZERO)
    }
}

impl<'a> core::iter::Sum<&'a Self> for Scalar {
    fn sum<I: Iterator<Item = &'a Self>>(iter: I) -> Self {
        iter.cloned().sum()
    }
}

impl core::iter::Sum for Point {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.reduce(core::ops::Add::add).unwrap_or(Self::IDENTITY)
    }
}

impl<'a> core::iter::Sum<&'a Self> for Point {
    fn sum<I: Iterator<Item = &'a Self>>(iter: I) -> Self {
        iter.cloned().sum()
    }
}
