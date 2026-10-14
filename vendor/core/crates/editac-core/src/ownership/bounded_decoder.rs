//! Capped decoding of the exact pinned v1 update format for structure v3 admission.
use std::{collections::HashMap, sync::Arc};
use yrs::encoding::read::{Error, Read};
use yrs::updates::decoder::{Decode, Decoder, DecoderV1};
use yrs::{Any, ClientID, ID, Number, Update};
struct Bounded<'a> {
    inner: DecoderV1<'a>,
    values: usize,
    structs: usize,
}
impl Read for Bounded<'_> {
    fn read_var<T: yrs::encoding::varint::VarInt>(&mut self) -> Result<T, Error> {
        if matches!(std::any::type_name::<T>(), "u32" | "usize") {
            let rest = self.inner.read_to_end()?;
            let mut cursor = yrs::encoding::read::Cursor::new(rest);
            let n: u64 = cursor.read_var()?;
            if n > 2097152 {
                return Err(limit());
            }
        }
        self.inner.read_var()
    }
    fn read_exact(&mut self, n: usize) -> Result<&[u8], Error> {
        self.inner.read_exact(n)
    }
    fn read_u8(&mut self) -> Result<u8, Error> {
        self.inner.read_u8()
    }
}
fn limit() -> Error {
    Error::Custom("resource-limit".into())
}
impl Bounded<'_> {
    fn any(&mut self, depth: usize) -> Result<Any, Error> {
        self.values += 1;
        if depth > 32 || self.values > 262144 {
            return Err(limit());
        };
        Ok(match self.read_u8()? {
            127 => Any::Undefined,
            126 => Any::Null,
            125 => Any::Number(Number::Int(self.read_var()?)),
            124 => Any::from(self.read_f32()? as f64),
            123 => Any::from(self.read_f64()?),
            122 => Any::Number(Number::Int(self.read_i64()?)),
            121 => Any::Bool(false),
            120 => Any::Bool(true),
            119 => Any::from(self.read_string()?),
            118 => {
                let n: usize = self.read_var()?;
                if n > 8192 {
                    return Err(limit());
                };
                let mut m = HashMap::new();
                for _ in 0..n {
                    let k = self.read_string()?.to_owned();
                    m.insert(k, self.any(depth + 1)?);
                }
                Any::Map(m.into())
            }
            117 => {
                let n: usize = self.read_var()?;
                if n > 8192 {
                    return Err(limit());
                };
                let mut a = Vec::new();
                for _ in 0..n {
                    a.push(self.any(depth + 1)?);
                }
                Any::Array(a.into())
            }
            116 => Any::Buffer(self.read_buf()?.into()),
            _ => return Err(Error::UnexpectedValue),
        })
    }
}
macro_rules! forward {
    ($name:ident,$ret:ty) => {
        fn $name(&mut self) -> Result<$ret, Error> {
            self.inner.$name()
        }
    };
}
impl Decoder for Bounded<'_> {
    fn reset_ds_cur_val(&mut self) {
        self.inner.reset_ds_cur_val()
    }
    forward!(read_ds_clock, u32);
    forward!(read_ds_len, u32);
    forward!(read_left_id, ID);
    forward!(read_right_id, ID);
    forward!(read_client, ClientID);
    forward!(read_parent_info, bool);
    forward!(read_type_ref, u8);
    forward!(read_key, Arc<str>);
    forward!(read_to_end, &[u8]);
    fn read_info(&mut self) -> Result<u8, Error> {
        self.structs += 1;
        if self.structs > 262144 {
            return Err(limit());
        };
        self.inner.read_info()
    }
    fn read_len(&mut self) -> Result<u32, Error> {
        let n = self.inner.read_len()?;
        if n > 1048576 {
            return Err(limit());
        };
        Ok(n)
    }
    fn read_any(&mut self) -> Result<Any, Error> {
        self.any(0)
    }
    fn read_json(&mut self) -> Result<Any, Error> {
        let value = self.inner.read_json()?;
        if !super::valid_json(&value, 0) {
            return Err(limit());
        };
        Ok(value)
    }
}
pub fn decode(bytes: &[u8]) -> super::Result<Update> {
    if bytes.len() > 16777216 {
        return Err("resource-limit");
    };
    let mut d = Bounded {
        inner: DecoderV1::from(bytes),
        values: 0,
        structs: 0,
    };
    let u = Update::decode(&mut d).map_err(|e| match e {
        Error::Custom(_) => "resource-limit",
        _ => "invalid-bytes",
    })?;
    if !d.read_to_end().map_err(|_| "invalid-bytes")?.is_empty() {
        return Err("invalid-bytes");
    };
    Ok(u)
}
