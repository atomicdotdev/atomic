use super::super::{
    CompactAtom, CompactEdgeUpdate, CompactGraphNode, CompactGraphOp, CompactInsertion,
    CompactNewEdge, CompactPosition, FormatError, FormatResult, GraphSectionPayload,
};
use super::wire::*;
use crate::change::Local;
use std::collections::HashSet;

struct Decoder<'a> {
    path: &'a str,
    start: u64,
    end: u64,
    control: &'a [u8],
    columns: [Numeric<'a>; ROLES],
    previous: [Delta; ROLES],
    dictionary: Vec<&'a str>,
    dictionary_end: usize,
    aux: &'a [u8],
    budget: usize,
}
#[derive(Default)]
struct Numeric<'a> {
    data: &'a [u8],
    count: u32,
    constant: Option<u64>,
}
fn bytes<'a>(input: &mut &'a [u8]) -> FormatResult<&'a [u8]> {
    let (v, rest) = postcard::take_from_bytes(input)?;
    *input = rest;
    Ok(v)
}
fn text<'a>(input: &mut &'a [u8]) -> FormatResult<&'a str> {
    let (v, rest) = postcard::take_from_bytes(input)?;
    *input = rest;
    Ok(v)
}
fn owned(value: &str) -> FormatResult<String> {
    let mut out = String::new();
    out.try_reserve(value.len())
        .map_err(|_| FormatError::GraphFieldLimitExceeded)?;
    out.push_str(value);
    Ok(out)
}
fn reserve<T>(n: usize) -> FormatResult<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n)
        .map_err(|_| FormatError::GraphFieldLimitExceeded)?;
    Ok(v)
}
impl<'a> Decoder<'a> {
    fn parse(payload: &'a [u8]) -> FormatResult<Self> {
        if payload.len() > MAX {
            return Err(FormatError::GraphFieldLimitExceeded);
        }
        let mut input = payload
            .strip_prefix(MAGIC)
            .ok_or_else(|| invalid("unsupported magic/version"))?;
        let path = text(&mut input)?;
        let start = get(&mut input)?;
        let end = get(&mut input)?;
        let control = bytes(&mut input)?;
        let n = usize::try_from(get(&mut input)?).map_err(|_| invalid("column count overflow"))?;
        if n > ROLES {
            return Err(invalid("column count limit"));
        }
        let mut columns: [Numeric<'_>; ROLES] = std::array::from_fn(|_| Numeric::default());
        let mut last = None;
        for _ in 0..n {
            let role =
                usize::try_from(get(&mut input)?).map_err(|_| invalid("column role overflow"))?;
            if role >= ROLES || last.is_some_and(|old| role <= old) {
                return Err(invalid("unknown, duplicate or unsorted role"));
            }
            let count =
                u32::try_from(get(&mut input)?).map_err(|_| invalid("numeric count overflow"))?;
            let (constant, rest): (Option<u64>, _) = postcard::take_from_bytes(input)?;
            input = rest;
            let data = bytes(&mut input)?;
            if count == 0
                || (constant.is_some() && !data.is_empty())
                || (constant.is_none()
                    && ((count as usize) > data.len()
                        || data.len() > (count as usize).saturating_mul(10)))
            {
                return Err(invalid("invalid numeric column/count"));
            }
            columns[role] = Numeric {
                data,
                count,
                constant,
            };
            last = Some(role);
        }
        let n =
            usize::try_from(get(&mut input)?).map_err(|_| invalid("dictionary count overflow"))?;
        if n > 1_000_000 || n > input.len() {
            return Err(invalid("dictionary count limit"));
        }
        let mut budget = ALLOC;
        charge(&mut budget, 1, 512)?;
        charge(&mut budget, path.len(), 1)?;
        let mut dictionary = reserve(n)?;
        let mut seen = HashSet::new();
        seen.try_reserve(n)
            .map_err(|_| FormatError::GraphFieldLimitExceeded)?;
        for _ in 0..n {
            let s = text(&mut input)?;
            charge(&mut budget, 1, 128)?;
            charge(&mut budget, s.len(), 2)?;
            if !seen.insert(s) {
                return Err(invalid("duplicate dictionary value"));
            }
            dictionary.push(s);
        }
        let aux = bytes(&mut input)?;
        charge(&mut budget, aux.len(), 1)?;
        if !input.is_empty() {
            return Err(invalid("trailing payload bytes"));
        }
        Ok(Self {
            path,
            start,
            end,
            control,
            columns,
            previous: std::array::from_fn(|_| Delta::default()),
            dictionary,
            dictionary_end: 0,
            aux,
            budget,
        })
    }
    fn n<T: TryFrom<u64>>(&mut self, role: usize) -> FormatResult<T> {
        let column = &mut self.columns[role];
        if column.count == 0 {
            return Err(invalid("numeric column exhausted"));
        }
        column.count -= 1;
        let n = if let Some(value) = column.constant {
            value
        } else {
            self.previous[role].read(&mut column.data)?
        };
        T::try_from(n).map_err(|_| invalid("numeric field overflow"))
    }
    fn position(&mut self, role: usize) -> FormatResult<CompactPosition> {
        Ok(CompactPosition {
            change: self.n(role)?,
            pos: self.n(role + 1)?,
        })
    }
    fn flag(&mut self) -> FormatResult<bool> {
        match get(&mut self.control)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid("invalid presence flag")),
        }
    }
    fn count(&mut self, cost: usize, available: usize) -> FormatResult<usize> {
        let n = usize::try_from(get(&mut self.control)?).map_err(|_| invalid("count overflow"))?;
        if n > available {
            return Err(invalid("count exceeds available data"));
        }
        charge(&mut self.budget, n, cost)?;
        Ok(n)
    }
    fn path(&mut self) -> FormatResult<String> {
        let id: usize = self.n(PATH)?;
        let path = self
            .dictionary
            .get(id)
            .ok_or_else(|| invalid("path dictionary index"))?;
        if id > self.dictionary_end {
            return Err(invalid("dictionary first-use gap"));
        }
        if id == self.dictionary_end {
            self.dictionary_end += 1;
        }
        charge(&mut self.budget, path.len(), 1)?;
        owned(path)
    }
    fn local(&mut self, role: usize) -> FormatResult<Local> {
        Ok(Local {
            path: self.path()?,
            line: self.n(LOCAL + role)?,
        })
    }
    fn aux<T: serde::de::DeserializeOwned>(&mut self) -> FormatResult<T> {
        let (value, rest) = postcard::take_from_bytes(self.aux)?;
        self.aux = rest;
        Ok(value)
    }
    fn positions(&mut self, role: usize) -> FormatResult<Vec<CompactPosition>> {
        let available = self.columns[role].count.min(self.columns[role + 1].count) as usize;
        let n = self.count(16, available)?;
        let mut out = reserve(n)?;
        for _ in 0..n {
            out.push(self.position(role)?);
        }
        Ok(out)
    }
    fn insertion(&mut self, r: usize) -> FormatResult<CompactInsertion> {
        Ok(CompactInsertion {
            flag: self.n(ins(r, 0))?,
            start: self.n(ins(r, 1))?,
            end: self.n(ins(r, 2))?,
            inode: self.position(ins(r, 3))?,
            predecessors: self.positions(ins(r, 5))?,
            successors: self.positions(ins(r, 7))?,
        })
    }
    fn edges(&mut self, r: usize) -> FormatResult<CompactEdgeUpdate> {
        let inode = self.position(edge(r, 0))?;
        let available = (2..10)
            .map(|f| self.columns[edge(r, f)].count)
            .min()
            .unwrap_or(0) as usize;
        let n = self.count(64, available)?;
        let mut edges = reserve(n)?;
        for _ in 0..n {
            edges.push(CompactNewEdge {
                previous: self.n(edge(r, 2))?,
                flag: self.n(edge(r, 3))?,
                from: self.position(edge(r, 4))?,
                to: CompactGraphNode {
                    change: self.n(edge(r, 6))?,
                    start: self.n(edge(r, 7))?,
                    end: self.n(edge(r, 8))?,
                },
                introduced_by: self.n(edge(r, 9))?,
            });
        }
        Ok(CompactEdgeUpdate { inode, edges })
    }
    fn optional_insertion(&mut self, r: usize) -> FormatResult<Option<CompactInsertion>> {
        if self.flag()? {
            Ok(Some(self.insertion(r)?))
        } else {
            Ok(None)
        }
    }
    fn optional_edges(&mut self, r: usize) -> FormatResult<Option<CompactEdgeUpdate>> {
        if self.flag()? {
            Ok(Some(self.edges(r)?))
        } else {
            Ok(None)
        }
    }
    fn op(&mut self) -> FormatResult<CompactGraphOp> {
        use CompactGraphOp::*;
        let op = match get(&mut self.control)? {
            0 => FileAdd {
                add_name: self.insertion(0)?,
                add_inode: self.insertion(1)?,
                contents: self.optional_insertion(2)?,
                path: self.path()?,
                encoding: self.aux()?,
            },
            1 => DirAdd {
                add_name: self.insertion(0)?,
                add_inode: self.insertion(1)?,
                path: self.path()?,
            },
            2 => DirDel {
                del: self.edges(0)?,
                path: self.path()?,
            },
            3 => DirUndel {
                undel: self.edges(2)?,
                path: self.path()?,
            },
            4 => FileDel {
                del: self.edges(0)?,
                contents: self.optional_edges(1)?,
                path: self.path()?,
                encoding: self.aux()?,
            },
            5 => FileUndel {
                undel: self.edges(2)?,
                contents: self.optional_edges(3)?,
                path: self.path()?,
                encoding: self.aux()?,
            },
            6 => FileMove {
                del: self.edges(4)?,
                add: self.insertion(5)?,
                path: self.path()?,
            },
            7 => {
                let change = match get(&mut self.control)? {
                    0 => CompactAtom::Insertion(self.insertion(3)?),
                    1 => CompactAtom::EdgeUpdate(self.edges(5)?),
                    _ => return Err(invalid("unknown atom")),
                };
                Edit {
                    change,
                    local: self.local(0)?,
                    encoding: self.aux()?,
                }
            }
            8 => Replacement {
                change: self.edges(6)?,
                replacement: self.insertion(4)?,
                local: self.local(1)?,
                encoding: self.aux()?,
            },
            9 => SolveNameConflict {
                name: self.edges(7)?,
                path: self.path()?,
            },
            10 => UnsolveNameConflict {
                name: self.edges(8)?,
                path: self.path()?,
            },
            11 => SolveOrderConflict {
                change: self.edges(9)?,
                local: self.local(2)?,
            },
            12 => UnsolveOrderConflict {
                change: self.edges(10)?,
                local: self.local(3)?,
            },
            13 => ResurrectZombies {
                change: self.edges(11)?,
                local: self.local(4)?,
                encoding: self.aux()?,
            },
            14 => AddRoot {
                name: self.insertion(6)?,
                inode: self.insertion(7)?,
            },
            15 => DelRoot {
                name: self.edges(12)?,
                inode: self.edges(13)?,
            },
            16 => SetAttr {
                inode: self.position(ATTR)?,
                path: self.path()?,
                value: self.aux()?,
            },
            _ => return Err(invalid("unknown graph operation")),
        };
        op.validate_serialized_name_conflict()
            .map_err(|reason| FormatError::InvalidGraphOp {
                operation: op.type_name(),
                reason,
            })?;
        Ok(op)
    }
}

/// Decode directly to typed graph operations with bounded collection/string
/// allocations. No postcard replay buffer or repository database is required.
pub fn decode(payload: &[u8]) -> FormatResult<GraphSectionPayload> {
    let mut d = Decoder::parse(payload)?;
    let n = d.count(512, d.control.len())?;
    let mut ops = reserve(n)?;
    for _ in 0..n {
        ops.push(d.op()?);
    }
    if !d.control.is_empty()
        || !d.aux.is_empty()
        || d.columns.iter().any(|s| !s.data.is_empty() || s.count != 0)
        || d.dictionary_end != d.dictionary.len()
    {
        return Err(invalid("unconsumed graph fields"));
    }
    Ok(GraphSectionPayload {
        path: owned(d.path)?,
        ops,
        content_start: d.start,
        content_end: d.end,
    })
}

const _: () = {
    assert!(std::mem::size_of::<CompactGraphOp>() <= 512);
    assert!(std::mem::size_of::<CompactPosition>() <= 16);
    assert!(std::mem::size_of::<CompactNewEdge>() <= 64);
};
