//! Budgeted legacy graph fallback for schema 5. Compound serde access is
//! guarded before allocating elements; preallocation hints never trust counts.
use super::super::{FormatError, FormatResult, GraphSectionPayload};
use super::wire::{invalid, ALLOC, MAX};
use serde::de::{
    self, Deserialize, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor,
};
use std::cell::Cell;

struct Budget {
    remaining: Cell<usize>,
    exceeded: Cell<bool>,
}
impl Budget {
    fn charge<E: de::Error>(&self, n: usize) -> Result<(), E> {
        match self.remaining.get().checked_sub(n) {
            Some(left) => {
                self.remaining.set(left);
                Ok(())
            }
            None => {
                self.exceeded.set(true);
                Err(E::custom("graph allocation budget"))
            }
        }
    }
}
struct Guard<'a, D> {
    inner: D,
    b: &'a Budget,
    depth: usize,
}
struct V<'a, T> {
    inner: T,
    b: &'a Budget,
    depth: usize,
    collection: bool,
}
struct Seed<'a, S> {
    inner: S,
    b: &'a Budget,
    depth: usize,
}
impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for Seed<'_, S> {
    type Value = S::Value;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        if self.depth > 64 {
            self.b.exceeded.set(true);
            return Err(de::Error::custom("graph nesting limit"));
        }
        self.inner.deserialize(Guard {
            inner: d,
            b: self.b,
            depth: self.depth,
        })
    }
}
macro_rules! primitive_visit{($($name:ident($t:ty)),*)=>{$(fn $name<E:de::Error>(self,v:$t)->Result<Self::Value,E>{self.inner.$name(v)})*}}
impl<'de, T: Visitor<'de>> Visitor<'de> for V<'_, T> {
    type Value = T::Value;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.expecting(f)
    }
    primitive_visit!(
        visit_bool(bool),
        visit_u8(u8),
        visit_u16(u16),
        visit_u32(u32),
        visit_u64(u64),
        visit_u128(u128),
        visit_i8(i8),
        visit_i16(i16),
        visit_i32(i32),
        visit_i64(i64),
        visit_i128(i128),
        visit_f32(f32),
        visit_f64(f64),
        visit_char(char)
    );
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        self.b.charge(v.len())?;
        self.inner.visit_str(v)
    }
    fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
        self.b.charge(v.len())?;
        self.inner.visit_borrowed_str(v)
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        self.b.charge(v.len())?;
        self.inner.visit_string(v)
    }
    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
        self.b.charge(v.len())?;
        self.inner.visit_bytes(v)
    }
    fn visit_borrowed_bytes<E: de::Error>(self, v: &'de [u8]) -> Result<Self::Value, E> {
        self.b.charge(v.len())?;
        self.inner.visit_borrowed_bytes(v)
    }
    fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
        self.b.charge(v.len())?;
        self.inner.visit_byte_buf(v)
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner.visit_none()
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner.visit_unit()
    }
    fn visit_some<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        self.inner.visit_some(Guard {
            inner: d,
            b: self.b,
            depth: self.depth + 1,
        })
    }
    fn visit_newtype_struct<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        self.inner.visit_newtype_struct(Guard {
            inner: d,
            b: self.b,
            depth: self.depth + 1,
        })
    }
    fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        self.inner.visit_seq(Access {
            inner: a,
            b: self.b,
            depth: self.depth + 1,
            collection: self.collection,
        })
    }
    fn visit_map<A: MapAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        self.inner.visit_map(Access {
            inner: a,
            b: self.b,
            depth: self.depth + 1,
            collection: true,
        })
    }
    fn visit_enum<A: EnumAccess<'de>>(self, a: A) -> Result<Self::Value, A::Error> {
        self.inner.visit_enum(Access {
            inner: a,
            b: self.b,
            depth: self.depth + 1,
            collection: false,
        })
    }
}
struct Access<'a, A> {
    inner: A,
    b: &'a Budget,
    depth: usize,
    collection: bool,
}
impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for Access<'_, A> {
    type Error = A::Error;
    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, Self::Error> {
        let cost = if self.collection && self.inner.size_hint() != Some(0) {
            512
        } else {
            0
        };
        self.b.charge(cost)?;
        let value = self.inner.next_element_seed(Seed {
            inner: seed,
            b: self.b,
            depth: self.depth,
        })?;
        if value.is_none() {
            self.b.remaining.set(self.b.remaining.get() + cost);
        }
        Ok(value)
    }
    fn size_hint(&self) -> Option<usize> {
        Some(0)
    }
}
impl<'de, A: MapAccess<'de>> MapAccess<'de> for Access<'_, A> {
    type Error = A::Error;
    fn next_key_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, Self::Error> {
        self.b.charge(512)?;
        self.inner.next_key_seed(Seed {
            inner: seed,
            b: self.b,
            depth: self.depth,
        })
    }
    fn next_value_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<S::Value, Self::Error> {
        self.inner.next_value_seed(Seed {
            inner: seed,
            b: self.b,
            depth: self.depth,
        })
    }
    fn size_hint(&self) -> Option<usize> {
        Some(0)
    }
}
impl<'a, 'de, A: EnumAccess<'de>> EnumAccess<'de> for Access<'a, A> {
    type Error = A::Error;
    type Variant = Access<'a, A::Variant>;
    fn variant_seed<S: DeserializeSeed<'de>>(
        self,
        seed: S,
    ) -> Result<(S::Value, Self::Variant), Self::Error> {
        let (value, variant) = self.inner.variant_seed(Seed {
            inner: seed,
            b: self.b,
            depth: self.depth,
        })?;
        Ok((
            value,
            Access {
                inner: variant,
                b: self.b,
                depth: self.depth,
                collection: false,
            },
        ))
    }
}
impl<'de, A: VariantAccess<'de>> VariantAccess<'de> for Access<'_, A> {
    type Error = A::Error;
    fn unit_variant(self) -> Result<(), Self::Error> {
        self.inner.unit_variant()
    }
    fn newtype_variant_seed<S: DeserializeSeed<'de>>(
        self,
        seed: S,
    ) -> Result<S::Value, Self::Error> {
        self.inner.newtype_variant_seed(Seed {
            inner: seed,
            b: self.b,
            depth: self.depth,
        })
    }
    fn tuple_variant<T: Visitor<'de>>(self, len: usize, v: T) -> Result<T::Value, Self::Error> {
        self.inner.tuple_variant(
            len,
            V {
                inner: v,
                b: self.b,
                depth: self.depth,
                collection: false,
            },
        )
    }
    fn struct_variant<T: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        v: T,
    ) -> Result<T::Value, Self::Error> {
        self.inner.struct_variant(
            fields,
            V {
                inner: v,
                b: self.b,
                depth: self.depth,
                collection: false,
            },
        )
    }
}
macro_rules! forward{($name:ident,$collection:expr $(,$arg:ident:$ty:ty)*)=>{
    fn $name<T:Visitor<'de>>(self,$($arg:$ty,)*visitor:T)->Result<T::Value,Self::Error>{
        if self.depth>64{self.b.exceeded.set(true);return Err(de::Error::custom("graph nesting limit"));}
        self.inner.$name($($arg,)*V{inner:visitor,b:self.b,depth:self.depth,collection:$collection})
    }
}}
impl<'de, D: de::Deserializer<'de>> de::Deserializer<'de> for Guard<'_, D> {
    type Error = D::Error;
    fn is_human_readable(&self) -> bool {
        self.inner.is_human_readable()
    }
    forward!(deserialize_any, true);
    forward!(deserialize_bool, false);
    forward!(deserialize_u8, false);
    forward!(deserialize_u16, false);
    forward!(deserialize_u32, false);
    forward!(deserialize_u64, false);
    forward!(deserialize_u128, false);
    forward!(deserialize_i8, false);
    forward!(deserialize_i16, false);
    forward!(deserialize_i32, false);
    forward!(deserialize_i64, false);
    forward!(deserialize_i128, false);
    forward!(deserialize_f32, false);
    forward!(deserialize_f64, false);
    forward!(deserialize_char, false);
    forward!(deserialize_str, false);
    forward!(deserialize_string, false);
    forward!(deserialize_bytes, false);
    forward!(deserialize_byte_buf, false);
    forward!(deserialize_option, false);
    forward!(deserialize_unit, false);
    forward!(deserialize_unit_struct,false,name:&'static str);
    forward!(deserialize_newtype_struct,false,name:&'static str);
    forward!(deserialize_seq, true);
    forward!(deserialize_tuple,false,len:usize);
    forward!(deserialize_tuple_struct,false,name:&'static str,len:usize);
    forward!(deserialize_map, true);
    forward!(deserialize_struct,false,name:&'static str,fields:&'static [&'static str]);
    forward!(deserialize_enum,false,name:&'static str,variants:&'static [&'static str]);
    forward!(deserialize_identifier, false);
    forward!(deserialize_ignored_any, true);
}

/// Budget the raw graph fallback used by schema 5; legacy schemas keep their
/// established reader. The same conservative budget is checked before writing.
pub fn decode(payload: &[u8]) -> FormatResult<GraphSectionPayload> {
    if payload.len() > MAX {
        return Err(FormatError::GraphFieldLimitExceeded);
    }
    let budget = Budget {
        remaining: Cell::new(ALLOC - 512),
        exceeded: Cell::new(false),
    };
    let mut decoder = postcard::Deserializer::from_bytes(payload);
    let result = GraphSectionPayload::deserialize(Guard {
        inner: &mut decoder,
        b: &budget,
        depth: 0,
    });
    let graph = match result {
        Ok(g) => g,
        Err(_) if budget.exceeded.get() => return Err(FormatError::GraphFieldLimitExceeded),
        Err(e) => return Err(e.into()),
    };
    if !decoder.finalize()?.is_empty() {
        return Err(invalid("trailing raw graph bytes"));
    }
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_fallback_roundtrips_and_bounds_malformed_allocations() {
        let graph = GraphSectionPayload::new("path".into(), vec![], 0, 0);
        let bytes = postcard::to_allocvec(&graph).unwrap();
        assert_eq!(decode(&bytes).unwrap(), graph);
        let graph = GraphSectionPayload::new("x".repeat(1024), vec![], 0, 0);
        let bytes = postcard::to_allocvec(&graph).unwrap();
        let budget = Budget {
            remaining: Cell::new(64),
            exceeded: Cell::new(false),
        };
        let mut de = postcard::Deserializer::from_bytes(&bytes);
        assert!(GraphSectionPayload::deserialize(Guard {
            inner: &mut de,
            b: &budget,
            depth: 0
        })
        .is_err());
        assert!(budget.exceeded.get());
        let mut forged = vec![0];
        super::super::wire::put(&mut forged, u64::MAX);
        forged.extend([0, 0]);
        assert!(decode(&forged).is_err());
    }
}
