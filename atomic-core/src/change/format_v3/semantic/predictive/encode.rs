use super::super::wire::{get, invalid, put, Columns, Delta, MAX_PAYLOAD};
use super::{predictor_v1 as p, wire::*};
use crate::change::format_v3::{FormatError, FormatResult};
use crate::change::FileOps;

#[derive(Default)]
struct Exceptions {
    count: u32,
    mask: Vec<u8>,
    values: Vec<u8>,
}
impl Exceptions {
    fn add(&mut self, actual: u64, predicted: u64) -> FormatResult<()> {
        let n = self.count as usize;
        if n.is_multiple_of(8) {
            self.mask.push(0);
        }
        if actual != predicted {
            self.mask[n / 8] |= 1 << (n % 8);
            put(&mut self.values, actual);
        }
        self.count = self
            .count
            .checked_add(1)
            .ok_or(FormatError::SemanticLimitExceeded)?;
        Ok(())
    }
}
struct Encoder<'a> {
    input: &'a [u8],
    kinds: &'a [u8],
    refs: &'a [u8],
    literals: &'a [u8],
    content: &'a [u8],
    previous: Delta,
    cursor: usize,
    controls: [Vec<u8>; 11],
    length: Exceptions,
    kind: Exceptions,
}
impl Encoder<'_> {
    fn n(&mut self, role: usize) -> FormatResult<u64> {
        let n = get(&mut self.input)?;
        put(&mut self.controls[role], n);
        Ok(n)
    }
    fn flag(&mut self, role: usize) -> FormatResult<()> {
        if self.n(role)? > 1 {
            return Err(invalid("predictive presence flag"));
        }
        Ok(())
    }
    fn count(&mut self, role: usize) -> FormatResult<usize> {
        let n = usize::try_from(self.n(role)?).map_err(|_| invalid("predictive count overflow"))?;
        if n > self.input.len() {
            return Err(invalid("predictive count exceeds control bytes"));
        }
        Ok(n)
    }
    fn bytes(&mut self, has_kind: bool) -> FormatResult<()> {
        let tag = self.n(REFERENCE)?;
        if tag > 2 {
            return Err(invalid("predictive reference tag"));
        }
        let len = usize::try_from(get(&mut self.input)?)
            .map_err(|_| invalid("predictive length overflow"))?;
        let at = if tag == 0 {
            self.cursor
        } else {
            usize::try_from(self.previous.read(&mut self.refs)?)
                .map_err(|_| invalid("predictive offset overflow"))?
        };
        let pool = if tag == 2 {
            self.literals
        } else {
            self.content
        };
        let tail = pool
            .get(at..)
            .ok_or_else(|| invalid("predictive offset outside pool"))?;
        self.length.add(len as u64, p::length(tail) as u64)?;
        let end = at
            .checked_add(len)
            .ok_or_else(|| invalid("predictive range overflow"))?;
        let bytes = pool
            .get(at..end)
            .ok_or_else(|| invalid("predictive range outside pool"))?;
        if tag != 2 {
            self.cursor = end;
        }
        if has_kind {
            let actual = get(&mut self.kinds)?;
            if p::from_code(actual).is_none() {
                return Err(invalid("unknown predictive kind"));
            }
            self.kind.add(actual, p::code(p::kind(bytes)))?;
        }
        Ok(())
    }
    fn leaves(&mut self) -> FormatResult<()> {
        for _ in 0..self.count(TOKENS)? {
            match self.n(LEAF)? {
                0 => {
                    self.flag(LEAF_AFTER)?;
                    self.bytes(true)?;
                }
                1 | 3 => {}
                2 => self.bytes(false)?,
                _ => return Err(invalid("unknown predictive leaf operation")),
            }
        }
        Ok(())
    }
    fn run(&mut self) -> FormatResult<()> {
        for _ in 0..self.count(FILES)? {
            for _ in 0..self.count(LINES)? {
                self.flag(OLD)?;
                self.flag(NEW)?;
                self.flag(RANGE)?;
                match self.n(BRANCH)? {
                    0 => {
                        self.flag(BRANCH_AFTER)?;
                        self.leaves()?;
                    }
                    1 => self.leaves()?,
                    2 => {
                        self.leaves()?;
                        self.leaves()?;
                    }
                    3 => {}
                    4 => self.flag(BRANCH_AFTER)?,
                    _ => return Err(invalid("unknown predictive branch operation")),
                }
            }
        }
        if !self.input.is_empty() || !self.kinds.is_empty() || !self.refs.is_empty() {
            return Err(invalid("unused predictive input"));
        }
        Ok(())
    }
}

pub(in crate::change::format_v3::semantic) fn encode(
    ops: &[FileOps],
    content: &[u8],
) -> FormatResult<Vec<u8>> {
    let c: Columns = super::super::encode::columns(ops, content, true)?;
    let mut e = Encoder {
        input: &c.control,
        kinds: &c.kinds,
        refs: &c.references,
        literals: &c.literals,
        content,
        previous: Delta::default(),
        cursor: 0,
        controls: std::array::from_fn(|_| vec![]),
        length: Exceptions::default(),
        kind: Exceptions::default(),
    };
    e.run()?;
    let Encoder {
        controls,
        length,
        kind,
        ..
    } = e;
    drop(c.control);
    drop(c.kinds);
    let payload = Payload {
        predictor: PREDICTOR,
        content_hash: c.content_hash,
        metadata: c.metadata,
        controls,
        branches: c.branches,
        leaves: c.leaves,
        old_numbers: c.old_numbers,
        new_numbers: c.new_numbers,
        positions: c.positions,
        references: c.references,
        literals: c.literals,
        length_count: length.count,
        length_mask: length.mask,
        length_exceptions: length.values,
        kind_count: kind.count,
        kind_mask: kind.mask,
        kind_exceptions: kind.values,
    };
    let mut out = MAGIC.to_vec();
    out.extend(postcard::to_allocvec(&payload)?);
    if out.len() > MAX_PAYLOAD {
        return Err(FormatError::SemanticLimitExceeded);
    }
    Ok(out)
}
