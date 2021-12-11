#![allow(dead_code)]

pub trait Leb128 {
    fn to_leb128_bytes(&self, out: &mut Vec<u8>);
    fn from_leb128_bytes(slice: &[u8]) -> Option<(Self, usize)>
    where
        Self: std::marker::Sized;
    fn from_leb128_it<'x, T>(it: T) -> Option<Self>
    where
        Self: std::marker::Sized,
        T: Iterator<Item = &'x u8>;
}

// Based on leb128.rs from rustc
macro_rules! impl_unsigned_leb128 {
    ($int_ty:ident) => {
        impl Leb128 for $int_ty {
            #[inline]
            fn to_leb128_bytes(&self, out: &mut Vec<u8>) {
                let mut value = *self;
                loop {
                    if value < 0x80 {
                        out.push(value as u8);
                        break;
                    } else {
                        out.push(((value & 0x7f) | 0x80) as u8);
                        value >>= 7;
                    }
                }
            }

            #[inline]
            fn from_leb128_bytes(slice: &[u8]) -> Option<($int_ty, usize)> {
                let mut result = 0;
                let mut shift = 0;
                let mut position = 0;
                loop {
                    let byte = *slice.get(position)?;
                    position += 1;
                    if (byte & 0x80) == 0 {
                        result |= (byte as $int_ty) << shift;
                        return Some((result, position));
                    } else {
                        result |= ((byte & 0x7F) as $int_ty) << shift;
                    }
                    shift += 7;
                }
            }

            #[inline]
            fn from_leb128_it<'x, T>(it: T) -> Option<$int_ty>
            where
                T: Iterator<Item = &'x u8>,
            {
                let mut result = 0;
                let mut shift = 0;
                for byte in it {
                    if (byte & 0x80) == 0 {
                        result |= (*byte as $int_ty) << shift;
                        return Some(result);
                    } else {
                        result |= ((byte & 0x7F) as $int_ty) << shift;
                    }
                    shift += 7;
                }
                None
            }
        }
    };
}

impl_unsigned_leb128!(u16);
impl_unsigned_leb128!(u32);
impl_unsigned_leb128!(u64);
impl_unsigned_leb128!(u128);
impl_unsigned_leb128!(usize);