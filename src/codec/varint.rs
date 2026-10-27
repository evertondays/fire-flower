//! Varint e o leitor de bytes usado pelos outros formatos.

use crate::Error;

pub fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

pub fn get_varint(bytes: &[u8]) -> Result<u64, Error> {
    let mut r = Reader::new(bytes);
    let v = r.varint()?;
    r.finish()?;
    Ok(v)
}

pub(super) struct Reader<'a> {
    pub(super) buf: &'a [u8],
    pub(super) pos: usize,
}

impl<'a> Reader<'a> {
    pub(super) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(super) fn byte(&mut self) -> Result<u8, Error> {
        let b = *self.buf.get(self.pos).ok_or_else(|| corrupt("valor truncado"))?;
        self.pos += 1;
        Ok(b)
    }

    /// A maioria dos valores (deltas, tf, posições) cabe em 1 byte.
    #[inline(always)]
    pub(super) fn varint(&mut self) -> Result<u64, Error> {
        if let Some(&b) = self.buf.get(self.pos)
            && b < 0x80
        {
            self.pos += 1;
            return Ok(u64::from(b));
        }
        self.varint_slow()
    }

    #[cold]
    fn varint_slow(&mut self) -> Result<u64, Error> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(corrupt("varint longo demais"))
    }

    pub(super) fn u32(&mut self) -> Result<u32, Error> {
        u32::try_from(self.varint()?).map_err(|_| corrupt("valor estourou u32"))
    }

    /// Pula `k` varints sem decodificar: conta os bytes de fim (bit alto zerado).
    #[inline]
    pub(super) fn skip_varints(&mut self, mut k: u64) -> Result<(), Error> {
        while k > 0 {
            let b = *self.buf.get(self.pos).ok_or_else(|| corrupt("valor truncado"))?;
            self.pos += 1;
            k -= u64::from(b < 0x80);
        }
        Ok(())
    }

    pub(super) fn finish(&self) -> Result<(), Error> {
        if self.pos == self.buf.len() { Ok(()) } else { Err(corrupt("bytes sobrando no valor")) }
    }
}

pub(super) fn corrupt(msg: &str) -> Error {
    Error::Corrupt(msg.to_string())
}
