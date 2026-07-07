//! Union types — generic tagged enums for composing heterogeneous types.
//!
//! Provides `Union2<A, B>`, `Union3<A, B, C>`, and `Union4<A, B, C, D>`,
//! each with named accessors (`.as_a()`, `.into_b()`, etc.) and common
//! trait derivations.
//!
//! Use the [`union!`] macro as syntactic sugar:
//!
//! ```ignore
//! type MyUnion = union!(String | i64 | bool);
//! // expands to Union3<String, i64, bool>
//! ```

use serde::{Deserialize, Serialize};

// ── Union2 ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Union2<A, B> {
    A(A),
    B(B),
}

impl<A, B> Union2<A, B> {
    pub fn from_a(a: A) -> Self {
        Self::A(a)
    }
    pub fn from_b(b: B) -> Self {
        Self::B(b)
    }

    pub fn is_a(&self) -> bool {
        matches!(self, Self::A(_))
    }
    pub fn is_b(&self) -> bool {
        matches!(self, Self::B(_))
    }

    pub fn as_a(&self) -> Option<&A> {
        match self {
            Self::A(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_b(&self) -> Option<&B> {
        match self {
            Self::B(b) => Some(b),
            _ => None,
        }
    }

    pub fn into_a(self) -> Option<A> {
        match self {
            Self::A(a) => Some(a),
            _ => None,
        }
    }
    pub fn into_b(self) -> Option<B> {
        match self {
            Self::B(b) => Some(b),
            _ => None,
        }
    }

    pub fn map_a<T>(self, f: impl FnOnce(A) -> T) -> Union2<T, B> {
        match self {
            Self::A(a) => Union2::A(f(a)),
            Self::B(b) => Union2::B(b),
        }
    }
    pub fn map_b<T>(self, f: impl FnOnce(B) -> T) -> Union2<A, T> {
        match self {
            Self::A(a) => Union2::A(a),
            Self::B(b) => Union2::B(f(b)),
        }
    }
}

// ── Union3 ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Union3<A, B, C> {
    A(A),
    B(B),
    C(C),
}

impl<A, B, C> Union3<A, B, C> {
    pub fn from_a(a: A) -> Self {
        Self::A(a)
    }
    pub fn from_b(b: B) -> Self {
        Self::B(b)
    }
    pub fn from_c(c: C) -> Self {
        Self::C(c)
    }

    pub fn is_a(&self) -> bool {
        matches!(self, Self::A(_))
    }
    pub fn is_b(&self) -> bool {
        matches!(self, Self::B(_))
    }
    pub fn is_c(&self) -> bool {
        matches!(self, Self::C(_))
    }

    pub fn as_a(&self) -> Option<&A> {
        match self {
            Self::A(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_b(&self) -> Option<&B> {
        match self {
            Self::B(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_c(&self) -> Option<&C> {
        match self {
            Self::C(c) => Some(c),
            _ => None,
        }
    }

    pub fn into_a(self) -> Option<A> {
        match self {
            Self::A(a) => Some(a),
            _ => None,
        }
    }
    pub fn into_b(self) -> Option<B> {
        match self {
            Self::B(b) => Some(b),
            _ => None,
        }
    }
    pub fn into_c(self) -> Option<C> {
        match self {
            Self::C(c) => Some(c),
            _ => None,
        }
    }
}

// ── Union4 ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Union4<A, B, C, D> {
    A(A),
    B(B),
    C(C),
    D(D),
}

impl<A, B, C, D> Union4<A, B, C, D> {
    pub fn from_a(a: A) -> Self {
        Self::A(a)
    }
    pub fn from_b(b: B) -> Self {
        Self::B(b)
    }
    pub fn from_c(c: C) -> Self {
        Self::C(c)
    }
    pub fn from_d(d: D) -> Self {
        Self::D(d)
    }

    pub fn is_a(&self) -> bool {
        matches!(self, Self::A(_))
    }
    pub fn is_b(&self) -> bool {
        matches!(self, Self::B(_))
    }
    pub fn is_c(&self) -> bool {
        matches!(self, Self::C(_))
    }
    pub fn is_d(&self) -> bool {
        matches!(self, Self::D(_))
    }

    pub fn as_a(&self) -> Option<&A> {
        match self {
            Self::A(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_b(&self) -> Option<&B> {
        match self {
            Self::B(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_c(&self) -> Option<&C> {
        match self {
            Self::C(c) => Some(c),
            _ => None,
        }
    }
    pub fn as_d(&self) -> Option<&D> {
        match self {
            Self::D(d) => Some(d),
            _ => None,
        }
    }

    pub fn into_a(self) -> Option<A> {
        match self {
            Self::A(a) => Some(a),
            _ => None,
        }
    }
    pub fn into_b(self) -> Option<B> {
        match self {
            Self::B(b) => Some(b),
            _ => None,
        }
    }
    pub fn into_c(self) -> Option<C> {
        match self {
            Self::C(c) => Some(c),
            _ => None,
        }
    }
    pub fn into_d(self) -> Option<D> {
        match self {
            Self::D(d) => Some(d),
            _ => None,
        }
    }
}

// ── union! macro ──────────────────────────────────────────────────────────────

/// Syntactic sugar for constructing union types:
///
/// ```ignore
/// type MyType = union!(String | i64);        // Union2<String, i64>
/// type MyType = union!(String | i64 | bool); // Union3<String, i64, bool>
/// ```
#[macro_export]
macro_rules! union {
    ($A:ty | $B:ty) => { $crate::union::Union2<$A, $B> };
    ($A:ty | $B:ty | $C:ty) => { $crate::union::Union3<$A, $B, $C> };
    ($A:ty | $B:ty | $C:ty | $D:ty) => { $crate::union::Union4<$A, $B, $C, $D> };
}
