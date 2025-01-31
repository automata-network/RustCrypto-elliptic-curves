//! Field arithmetic modulo p = 2^{224}(2^{32} − 1) + 2^{192} + 2^{96} − 1

#![allow(clippy::assign_op_pattern, clippy::op_ref)]

#[cfg_attr(
    all(target_os = "zkvm", target_arch = "riscv32"),
    path = "field/field_risc0.rs"
)]
#[cfg_attr(
    all(
        not(all(target_os = "zkvm", target_arch = "riscv32")),
        target_pointer_width = "32"
    ),
    path = "field/field32.rs"
)]
#[cfg_attr(target_pointer_width = "64", path = "field/field64.rs")]
mod field_impl;

use crate::FieldBytes;
use core::{
    iter::{Product, Sum},
    ops::{Add, AddAssign, Mul, MulAssign, Neg, Sub, SubAssign},
};
use elliptic_curve::ops::Invert;
use elliptic_curve::{
    bigint::{ArrayEncoding, U256},
    ff::{Field, PrimeField},
    rand_core::RngCore,
    subtle::{Choice, ConditionallySelectable, ConstantTimeEq, ConstantTimeLess, CtOption},
    zeroize::DefaultIsZeroes,
};

const MODULUS_HEX: &str = "ffffffff00000001000000000000000000000000ffffffffffffffffffffffff";

/// Constant representing the modulus
/// p = 2^{224}(2^{32} − 1) + 2^{192} + 2^{96} − 1
pub const MODULUS: FieldElement = FieldElement(U256::from_be_hex(MODULUS_HEX));

/// An element in the finite field modulo p = 2^{224}(2^{32} − 1) + 2^{192} + 2^{96} − 1.
///
/// The internal representation is in little-endian order.
#[derive(Clone, Copy, Debug)]
pub struct FieldElement(pub(crate) U256);

impl FieldElement {
    /// Zero element.
    pub const ZERO: Self = Self(U256::ZERO);

    /// Multiplicative identity.
    pub const ONE: Self = Self(U256::ONE);

    /// Attempts to parse the given byte array as an SEC1-encoded field element.
    ///
    /// Returns None if the byte array does not contain a big-endian integer in the range
    /// [0, p).
    pub fn from_bytes(bytes: FieldBytes) -> CtOption<Self> {
        Self::from_uint(U256::from_be_byte_array(bytes))
    }

    /// Returns the SEC1 encoding of this field element.
    pub fn to_bytes(self) -> FieldBytes {
        self.0.to_be_byte_array()
    }

    /// Decode [`FieldElement`] from [`U256`]
    pub fn from_uint(uint: U256) -> CtOption<Self> {
        let is_some = uint.ct_lt(&MODULUS.0);
        CtOption::new(Self(uint), is_some)
    }

    /// Convert a `u64` into a [`FieldElement`].
    pub fn from_u64(w: u64) -> Self {
        Self(U256::from_u64(w))
    }

    /// Parse a [`FieldElement`] from big endian hex-encoded bytes.
    ///
    /// Does *not* perform a check that the field element does not overflow the order.
    ///
    /// This method is primarily intended for defining internal constants.
    #[allow(dead_code)]
    pub(crate) fn from_hex(hex: &str) -> Self {
        Self(U256::from_be_hex(hex))
    }

    /// Determine if this `FieldElement` is zero.
    ///
    /// # Returns
    ///
    /// If zero, return `Choice(1)`.  Otherwise, return `Choice(0)`.
    pub fn is_zero(&self) -> Choice {
        self.ct_eq(&FieldElement::ZERO)
    }

    /// Determine if this `FieldElement` is odd in the SEC1 sense: `self mod 2 == 1`.
    ///
    /// # Returns
    ///
    /// If odd, return `Choice(1)`.  Otherwise, return `Choice(0)`.
    pub fn is_odd(&self) -> Choice {
        let bytes = self.to_bytes();
        (bytes[31] & 1).into()
    }

    /// Returns self + rhs mod p
    pub fn add(&self, rhs: &Self) -> Self {
        Self(field_impl::add(self.0, rhs.0))
    }

    /// Multiplies by a single-limb integer.
    /// Multiplies the magnitude by the same value.
    pub fn mul_single(&self, rhs: u32) -> Self {
        Self(field_impl::mul_single(self.0, rhs))
    }

    /// Returns 2*self.
    pub fn double(&self) -> Self {
        if cfg!(all(target_os = "zkvm", target_arch = "riscv32")) {
            self.mul_single(2)
        } else {
            self.add(self)
        }
    }

    /// Returns self - rhs mod p
    pub fn sub(&self, rhs: &Self) -> Self {
        Self(field_impl::sub(self.0, rhs.0))
    }

    /// Negate element.
    pub fn neg(&self) -> Self {
        Self::sub(&Self::ZERO, self)
    }

    /// Returns self * rhs mod p
    pub fn multiply(&self, rhs: &Self) -> Self {
        Self(field_impl::mul(self.0, rhs.0))
    }

    /// Returns self * self mod p
    pub fn square(&self) -> Self {
        // Schoolbook multiplication.
        self.multiply(self)
    }

    /// Returns self^(2^n) mod p
    fn sqn(&self, n: usize) -> Self {
        let mut x = *self;
        let mut i = 0;
        while i < n {
            x = x.square();
            i += 1;
        }
        x
    }

    /// Returns `self^by`, where `by` is a little-endian integer exponent.
    ///
    /// **This operation is variable time with respect to the exponent.** If the exponent
    /// is fixed, this operation is effectively constant time.
    pub fn pow_vartime(&self, by: &[u64; 4]) -> Self {
        let mut res = Self::ONE;
        for e in by.iter().rev() {
            for i in (0..64).rev() {
                res = res.square();

                if ((*e >> i) & 1) == 1 {
                    res = res * self;
                }
            }
        }
        res
    }

    /// Returns the multiplicative inverse of self, if self is non-zero.
    pub fn invert(&self) -> CtOption<Self> {
        CtOption::new(self.invert_unchecked(), !self.is_zero())
    }

    /// Returns the multiplicative inverse of self.
    ///
    /// Does not check that self is non-zero.
    pub fn invert_unchecked(&self) -> Self {
        // We need to find b such that b * a ≡ 1 mod p. As we are in a prime
        // field, we can apply Fermat's Little Theorem:
        //
        //    a^p         ≡ a mod p
        //    a^(p-1)     ≡ 1 mod p
        //    a^(p-2) * a ≡ 1 mod p
        //
        // Thus inversion can be implemented with a single exponentiation.
        let t111 = self.multiply(&self.multiply(&self.square()).square());
        let t111111 = t111.multiply(&t111.sqn(3));
        let x15 = t111111.sqn(6).multiply(&t111111).sqn(3).multiply(&t111);
        let x16 = x15.square().multiply(self);
        let i53 = x16.sqn(16).multiply(&x16).sqn(15);
        let x47 = x15.multiply(&i53);
        x47.multiply(&i53.sqn(17).multiply(self).sqn(143).multiply(&x47).sqn(47))
            .sqn(2)
            .multiply(self)
    }

    /// Returns the square root of self mod p, or `None` if no square root exists.
    pub fn sqrt(&self) -> CtOption<Self> {
        // We need to find alpha such that alpha^2 = beta mod p. For secp256r1,
        // p ≡ 3 mod 4. By Euler's Criterion, beta^(p-1)/2 ≡ 1 mod p. So:
        //
        //     alpha^2 = beta beta^((p - 1) / 2) mod p ≡ beta^((p + 1) / 2) mod p
        //     alpha = ± beta^((p + 1) / 4) mod p
        //
        // Thus sqrt can be implemented with a single exponentiation.

        let t11 = self.mul(&self.square());
        let t1111 = t11.mul(&t11.sqn(2));
        let t11111111 = t1111.mul(t1111.sqn(4));
        let x16 = t11111111.sqn(8).mul(t11111111);
        let sqrt = x16
            .sqn(16)
            .mul(x16)
            .sqn(32)
            .mul(self)
            .sqn(96)
            .mul(self)
            .sqn(94);

        CtOption::new(
            sqrt,
            (&sqrt * &sqrt).ct_eq(self), // Only return Some if it's the square root.
        )
    }
}

impl Field for FieldElement {
    const ZERO: Self = Self::ZERO;
    const ONE: Self = Self::ONE;

    fn random(mut rng: impl RngCore) -> Self {
        let mut bytes = FieldBytes::default();

        loop {
            rng.fill_bytes(&mut bytes);
            if let Some(fe) = Self::from_bytes(bytes).into() {
                return fe;
            }
        }
    }

    #[must_use]
    fn square(&self) -> Self {
        self.square()
    }

    #[must_use]
    fn double(&self) -> Self {
        self.double()
    }

    fn invert(&self) -> CtOption<Self> {
        self.invert()
    }

    fn sqrt(&self) -> CtOption<Self> {
        self.sqrt()
    }

    fn sqrt_ratio(num: &Self, div: &Self) -> (Choice, Self) {
        elliptic_curve::ff::helpers::sqrt_ratio_generic(num, div)
    }
}

impl PrimeField for FieldElement {
    type Repr = FieldBytes;

    const MODULUS: &'static str = MODULUS_HEX;
    const NUM_BITS: u32 = 256;
    const CAPACITY: u32 = 255;
    const TWO_INV: Self = Self(U256::from_be_hex(
        "7FFFFFFF80000000800000000000000000000000800000000000000000000000",
    ));
    const MULTIPLICATIVE_GENERATOR: Self = Self(U256::from_be_hex(
        "0000000000000000000000000000000000000000000000000000000000000006",
    ));
    const S: u32 = 1;
    const ROOT_OF_UNITY: Self = Self(U256::from_be_hex(
        "FFFFFFFF00000001000000000000000000000000FFFFFFFFFFFFFFFFFFFFFFFE",
    ));
    const ROOT_OF_UNITY_INV: Self = Self(U256::from_be_hex(
        "FFFFFFFF00000001000000000000000000000000FFFFFFFFFFFFFFFFFFFFFFFE",
    ));
    const DELTA: Self = Self(U256::from_be_hex(
        "0000000000000000000000000000000000000000000000000000000000000024",
    ));

    fn from_repr(bytes: FieldBytes) -> CtOption<Self> {
        Self::from_bytes(bytes)
    }

    fn to_repr(&self) -> FieldBytes {
        self.to_bytes()
    }

    fn is_odd(&self) -> Choice {
        self.is_odd()
    }
}

impl ConditionallySelectable for FieldElement {
    #[inline(always)]
    fn conditional_select(a: &Self, b: &Self, choice: Choice) -> Self {
        Self(U256::conditional_select(&a.0, &b.0, choice))
    }
}

impl ConstantTimeEq for FieldElement {
    fn ct_eq(&self, other: &Self) -> Choice {
        self.0.ct_eq(&other.0)
    }
}

impl Default for FieldElement {
    fn default() -> Self {
        FieldElement::ZERO
    }
}

impl DefaultIsZeroes for FieldElement {}

impl Eq for FieldElement {}

impl From<u64> for FieldElement {
    fn from(n: u64) -> FieldElement {
        Self(U256::from(n))
    }
}

impl PartialEq for FieldElement {
    fn eq(&self, other: &Self) -> bool {
        self.ct_eq(other).into()
    }
}

impl Invert for FieldElement {
    type Output = CtOption<Self>;

    fn invert(&self) -> CtOption<Self> {
        self.invert()
    }
}

impl Add<FieldElement> for FieldElement {
    type Output = FieldElement;

    fn add(self, other: FieldElement) -> FieldElement {
        FieldElement::add(&self, &other)
    }
}

impl Add<&FieldElement> for FieldElement {
    type Output = FieldElement;

    fn add(self, other: &FieldElement) -> FieldElement {
        FieldElement::add(&self, other)
    }
}

impl Add<&FieldElement> for &FieldElement {
    type Output = FieldElement;

    fn add(self, other: &FieldElement) -> FieldElement {
        FieldElement::add(self, other)
    }
}

impl AddAssign<FieldElement> for FieldElement {
    fn add_assign(&mut self, other: FieldElement) {
        *self = FieldElement::add(self, &other);
    }
}

impl AddAssign<&FieldElement> for FieldElement {
    fn add_assign(&mut self, other: &FieldElement) {
        *self = FieldElement::add(self, other);
    }
}

impl Sub<FieldElement> for FieldElement {
    type Output = FieldElement;

    fn sub(self, other: FieldElement) -> FieldElement {
        FieldElement::sub(&self, &other)
    }
}

impl Sub<&FieldElement> for FieldElement {
    type Output = FieldElement;

    fn sub(self, other: &FieldElement) -> FieldElement {
        FieldElement::sub(&self, other)
    }
}

impl Sub<&FieldElement> for &FieldElement {
    type Output = FieldElement;

    fn sub(self, other: &FieldElement) -> FieldElement {
        FieldElement::sub(self, other)
    }
}

impl SubAssign<FieldElement> for FieldElement {
    fn sub_assign(&mut self, other: FieldElement) {
        *self = FieldElement::sub(self, &other);
    }
}

impl SubAssign<&FieldElement> for FieldElement {
    fn sub_assign(&mut self, other: &FieldElement) {
        *self = FieldElement::sub(self, other);
    }
}

impl Mul<FieldElement> for FieldElement {
    type Output = FieldElement;

    fn mul(self, other: FieldElement) -> FieldElement {
        FieldElement::multiply(&self, &other)
    }
}

impl Mul<&FieldElement> for FieldElement {
    type Output = FieldElement;

    fn mul(self, other: &FieldElement) -> FieldElement {
        FieldElement::multiply(&self, other)
    }
}

impl Mul<&FieldElement> for &FieldElement {
    type Output = FieldElement;

    fn mul(self, other: &FieldElement) -> FieldElement {
        FieldElement::multiply(self, other)
    }
}

impl MulAssign<FieldElement> for FieldElement {
    fn mul_assign(&mut self, other: FieldElement) {
        *self = FieldElement::multiply(self, &other);
    }
}

impl MulAssign<&FieldElement> for FieldElement {
    fn mul_assign(&mut self, other: &FieldElement) {
        *self = FieldElement::multiply(self, other);
    }
}

impl Neg for FieldElement {
    type Output = FieldElement;

    fn neg(self) -> FieldElement {
        FieldElement::ZERO - &self
    }
}

impl Neg for &FieldElement {
    type Output = FieldElement;

    fn neg(self) -> FieldElement {
        FieldElement::ZERO - self
    }
}

impl Sum for FieldElement {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.reduce(Add::add).unwrap_or(Self::ZERO)
    }
}

impl<'a> Sum<&'a FieldElement> for FieldElement {
    fn sum<I: Iterator<Item = &'a FieldElement>>(iter: I) -> Self {
        iter.copied().sum()
    }
}

impl Product for FieldElement {
    fn product<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.reduce(Mul::mul).unwrap_or(Self::ONE)
    }
}

impl<'a> Product<&'a FieldElement> for FieldElement {
    fn product<I: Iterator<Item = &'a Self>>(iter: I) -> Self {
        iter.copied().product()
    }
}

#[cfg(test)]
mod tests {
    use super::FieldElement;
    use crate::{test_vectors::field::DBL_TEST_VECTORS, FieldBytes};
    use core::ops::Mul;
    use elliptic_curve::ff::PrimeField;

    use crate::U256;
    use proptest::{num::u64::ANY, prelude::*};

    #[test]
    fn zero_is_additive_identity() {
        let zero = FieldElement::ZERO;
        let one = FieldElement::ONE;
        assert_eq!(zero.add(&zero), zero);
        assert_eq!(one.add(&zero), one);
    }

    #[test]
    fn root_of_unity_constant() {
        let root_of_unity = FieldElement::from_hex(
            "ffffffff00000001000000000000000000000000fffffffffffffffffffffffe",
        );
        let root_of_unity_inv = root_of_unity.invert_unchecked();
        assert_eq!((root_of_unity * root_of_unity_inv), FieldElement::ONE);
        assert_eq!(root_of_unity, FieldElement::ROOT_OF_UNITY);
        assert_eq!(root_of_unity_inv, FieldElement::ROOT_OF_UNITY_INV);
    }

    #[test]
    fn two_inv_constant() {
        let two = FieldElement::from_u64(2);
        let two_inv = FieldElement::from_u64(2).invert_unchecked();
        assert_eq!((two * two_inv), FieldElement::ONE);
        assert_eq!(two_inv, FieldElement::TWO_INV);
    }

    #[test]
    fn multiplicative_generator_constant() {
        let multiplicative_generator = FieldElement::from_u64(6);
        assert_eq!(
            multiplicative_generator,
            FieldElement::MULTIPLICATIVE_GENERATOR
        );
    }

    #[test]
    fn delta_constant() {
        let delta = FieldElement::from_u64(36);
        assert_eq!(delta, FieldElement::DELTA);
    }

    #[test]
    fn one_is_multiplicative_identity() {
        let one = FieldElement::ONE;
        assert_eq!(one.mul(&one), one);
    }

    #[test]
    fn from_bytes() {
        assert_eq!(
            FieldElement::from_bytes(FieldBytes::default()).unwrap(),
            FieldElement::ZERO
        );
        assert_eq!(
            FieldElement::from_bytes(
                [
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    0, 0, 0, 0, 0, 1
                ]
                .into()
            )
            .unwrap(),
            FieldElement::ONE
        );
        assert!(bool::from(
            FieldElement::from_bytes([0xff; 32].into()).is_none()
        ));
    }

    #[test]
    fn to_bytes() {
        assert_eq!(FieldElement::ZERO.to_bytes(), FieldBytes::default());
        assert_eq!(
            FieldElement::ONE.to_bytes(),
            [
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 1
            ]
            .into()
        );
    }

    #[test]
    fn repeated_add() {
        let mut r = FieldElement::ONE;
        for i in 0..DBL_TEST_VECTORS.len() {
            assert_eq!(r.to_bytes(), DBL_TEST_VECTORS[i].into());
            r = r + &r;
        }
    }

    #[test]
    fn repeated_double() {
        let mut r = FieldElement::ONE;
        for i in 0..DBL_TEST_VECTORS.len() {
            assert_eq!(r.to_bytes(), DBL_TEST_VECTORS[i].into());
            r = r.double();
        }
    }

    #[test]
    fn repeated_mul() {
        let mut r = FieldElement::ONE;
        let two = r + &r;
        for i in 0..DBL_TEST_VECTORS.len() {
            assert_eq!(r.to_bytes(), DBL_TEST_VECTORS[i].into());
            r = r * &two;
        }
    }

    #[test]
    fn negation() {
        let two = FieldElement::ONE.double();
        let neg_two = -two;
        assert_eq!(two + &neg_two, FieldElement::ZERO);
        assert_eq!(-neg_two, two);
    }

    #[test]
    fn pow_vartime() {
        let one = FieldElement::ONE;
        let two = one + &one;
        let four = two.square();
        assert_eq!(two.pow_vartime(&[2, 0, 0, 0]), four);
    }

    #[test]
    fn invert() {
        // assert!(bool::from(FieldElement::ZERO.invert().is_none()));

        let one = FieldElement::ONE;
        assert_eq!(one.invert().unwrap(), one);

        let two = one + &one;
        let inv_two = two.invert().unwrap();
        assert_eq!(two * &inv_two, one);
    }

    #[test]
    fn sqrt() {
        let one = FieldElement::ONE;
        let two = one + &one;
        let four = two.square();
        assert_eq!(four.sqrt().unwrap(), two);
    }

    #[cfg(target_pointer_width = "64")]
    proptest! {
        /// This checks behaviour well within the field ranges, because it doesn't set the
        /// highest limb.
        #[test]
        fn add_then_sub(
            a0 in ANY,
            a1 in ANY,
            a2 in ANY,
            b0 in ANY,
            b1 in ANY,
            b2 in ANY,
        ) {
            let a = FieldElement(U256::from_words([a0, a1, a2, 0]));
            let b = FieldElement(U256::from_words([b0, b1, b2, 0]));
            assert_eq!(a.add(&b).sub(&a), b);
        }
    }
}
