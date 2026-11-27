use super::super::{
    CompactAtom, CompactEdgeUpdate, CompactGraphOp, CompactInsertion, CompactPosition, FormatError,
    FormatResult, GraphSectionPayload,
};
use super::wire::*;
use crate::change::Local;
use std::collections::{BTreeMap, HashMap};

struct Encoder {
    control: Vec<u8>,
    columns: BTreeMap<usize, (Delta, Vec<u8>, u32, Option<u64>)>,
    dictionary: Vec<String>,
    paths: HashMap<String, u32>,
    aux: Vec<u8>,
    budget: usize,
}
impl Encoder {
    fn n(&mut self, role: usize, n: u64) {
        let (previous, data, count, constant) = self.columns.entry(role).or_default();
        if *count == 0 {
            *constant = Some(n);
        } else if *constant != Some(n) {
            *constant = None;
        }
        *count += 1;
        previous.write(data, n);
    }
    fn position(&mut self, role: usize, p: CompactPosition) {
        self.n(role, p.change.into());
        self.n(role + 1, p.pos.into());
    }
    fn path(&mut self, path: &str) -> FormatResult<()> {
        charge(&mut self.budget, path.len(), 1)?;
        let id = if let Some(&id) = self.paths.get(path) {
            id
        } else {
            if self.dictionary.len() >= 1_000_000 {
                return Err(FormatError::GraphFieldLimitExceeded);
            }
            charge(&mut self.budget, 1, 128)?;
            charge(&mut self.budget, path.len(), 2)?;
            let id = self.dictionary.len() as u32;
            self.dictionary.push(path.into());
            self.paths.insert(path.into(), id);
            id
        };
        self.n(PATH, id.into());
        Ok(())
    }
    fn local(&mut self, role: usize, local: &Local) -> FormatResult<()> {
        self.path(&local.path)?;
        self.n(LOCAL + role, local.line);
        Ok(())
    }
    fn aux<T: serde::Serialize>(&mut self, value: &T) -> FormatResult<()> {
        let bytes = postcard::to_allocvec(value)?;
        charge(&mut self.budget, bytes.len(), 1)?;
        self.aux.extend(bytes);
        Ok(())
    }
    fn insertion(&mut self, r: usize, v: &CompactInsertion) -> FormatResult<()> {
        self.n(ins(r, 0), v.flag.into());
        self.n(ins(r, 1), v.start.into());
        self.n(ins(r, 2), v.end.into());
        self.position(ins(r, 3), v.inode);
        charge(&mut self.budget, v.predecessors.len(), 16)?;
        put(&mut self.control, v.predecessors.len() as u64);
        for p in &v.predecessors {
            self.position(ins(r, 5), *p);
        }
        charge(&mut self.budget, v.successors.len(), 16)?;
        put(&mut self.control, v.successors.len() as u64);
        for p in &v.successors {
            self.position(ins(r, 7), *p);
        }
        Ok(())
    }
    fn edges(&mut self, r: usize, v: &CompactEdgeUpdate) -> FormatResult<()> {
        self.position(edge(r, 0), v.inode);
        charge(&mut self.budget, v.edges.len(), 64)?;
        put(&mut self.control, v.edges.len() as u64);
        for e in &v.edges {
            self.n(edge(r, 2), e.previous.into());
            self.n(edge(r, 3), e.flag.into());
            self.position(edge(r, 4), e.from);
            self.n(edge(r, 6), e.to.change.into());
            self.n(edge(r, 7), e.to.start.into());
            self.n(edge(r, 8), e.to.end.into());
            self.n(edge(r, 9), e.introduced_by.into());
        }
        Ok(())
    }
    fn optional_insertion(&mut self, r: usize, v: &Option<CompactInsertion>) -> FormatResult<()> {
        put(&mut self.control, u64::from(v.is_some()));
        if let Some(v) = v {
            self.insertion(r, v)?;
        }
        Ok(())
    }
    fn optional_edges(&mut self, r: usize, v: &Option<CompactEdgeUpdate>) -> FormatResult<()> {
        put(&mut self.control, u64::from(v.is_some()));
        if let Some(v) = v {
            self.edges(r, v)?;
        }
        Ok(())
    }
    fn op(&mut self, op: &CompactGraphOp) -> FormatResult<()> {
        op.validate_serialized_name_conflict()
            .map_err(|reason| FormatError::InvalidGraphOp {
                operation: op.type_name(),
                reason,
            })?;
        use CompactGraphOp::*;
        match op {
            FileAdd {
                add_name,
                add_inode,
                contents,
                path,
                encoding,
            } => {
                put(&mut self.control, 0);
                self.insertion(0, add_name)?;
                self.insertion(1, add_inode)?;
                self.optional_insertion(2, contents)?;
                self.path(path)?;
                self.aux(encoding)?;
            }
            DirAdd {
                add_name,
                add_inode,
                path,
            } => {
                put(&mut self.control, 1);
                self.insertion(0, add_name)?;
                self.insertion(1, add_inode)?;
                self.path(path)?;
            }
            DirDel { del, path } => {
                put(&mut self.control, 2);
                self.edges(0, del)?;
                self.path(path)?;
            }
            DirUndel { undel, path } => {
                put(&mut self.control, 3);
                self.edges(2, undel)?;
                self.path(path)?;
            }
            FileDel {
                del,
                contents,
                path,
                encoding,
            } => {
                put(&mut self.control, 4);
                self.edges(0, del)?;
                self.optional_edges(1, contents)?;
                self.path(path)?;
                self.aux(encoding)?;
            }
            FileUndel {
                undel,
                contents,
                path,
                encoding,
            } => {
                put(&mut self.control, 5);
                self.edges(2, undel)?;
                self.optional_edges(3, contents)?;
                self.path(path)?;
                self.aux(encoding)?;
            }
            FileMove { del, add, path } => {
                put(&mut self.control, 6);
                self.edges(4, del)?;
                self.insertion(5, add)?;
                self.path(path)?;
            }
            Edit {
                change,
                local,
                encoding,
            } => {
                put(&mut self.control, 7);
                match change {
                    CompactAtom::Insertion(v) => {
                        put(&mut self.control, 0);
                        self.insertion(3, v)?;
                    }
                    CompactAtom::EdgeUpdate(v) => {
                        put(&mut self.control, 1);
                        self.edges(5, v)?;
                    }
                }
                self.local(0, local)?;
                self.aux(encoding)?;
            }
            Replacement {
                change,
                replacement,
                local,
                encoding,
            } => {
                put(&mut self.control, 8);
                self.edges(6, change)?;
                self.insertion(4, replacement)?;
                self.local(1, local)?;
                self.aux(encoding)?;
            }
            SolveNameConflict { name, path } => {
                put(&mut self.control, 9);
                self.edges(7, name)?;
                self.path(path)?;
            }
            UnsolveNameConflict { name, path } => {
                put(&mut self.control, 10);
                self.edges(8, name)?;
                self.path(path)?;
            }
            SolveOrderConflict { change, local } => {
                put(&mut self.control, 11);
                self.edges(9, change)?;
                self.local(2, local)?;
            }
            UnsolveOrderConflict { change, local } => {
                put(&mut self.control, 12);
                self.edges(10, change)?;
                self.local(3, local)?;
            }
            ResurrectZombies {
                change,
                local,
                encoding,
            } => {
                put(&mut self.control, 13);
                self.edges(11, change)?;
                self.local(4, local)?;
                self.aux(encoding)?;
            }
            AddRoot { name, inode } => {
                put(&mut self.control, 14);
                self.insertion(6, name)?;
                self.insertion(7, inode)?;
            }
            DelRoot { name, inode } => {
                put(&mut self.control, 15);
                self.edges(12, name)?;
                self.edges(13, inode)?;
            }
            SetAttr { inode, path, value } => {
                put(&mut self.control, 16);
                self.position(ATTR, *inode);
                self.path(path)?;
                self.aux(value)?;
            }
            #[cfg(feature = "prototype-context-completion")]
            PrototypeContext { .. } => return Err(FormatError::UnsupportedGraphFieldOp),
        }
        Ok(())
    }
}

/// Encode a graph section with frozen v1 field roles. Returns a decompressed
/// section payload; ordinary section framing supplies compression and hashing.
pub fn encode(graph: &GraphSectionPayload) -> FormatResult<Vec<u8>> {
    let mut e = Encoder {
        control: vec![],
        columns: BTreeMap::new(),
        dictionary: vec![],
        paths: HashMap::new(),
        aux: vec![],
        budget: ALLOC,
    };
    charge(&mut e.budget, 1, 512)?;
    charge(&mut e.budget, graph.path.len(), 1)?;
    charge(&mut e.budget, graph.ops.len(), 512)?;
    put(&mut e.control, graph.ops.len() as u64);
    for op in &graph.ops {
        e.op(op)?;
    }
    let columns = e
        .columns
        .into_iter()
        .map(|(role, (_, data, count, constant))| Column {
            role: role as u16,
            count,
            constant,
            data: if constant.is_some() { vec![] } else { data },
        })
        .collect();
    let payload = Payload {
        path: &graph.path,
        start: graph.content_start,
        end: graph.content_end,
        control: e.control,
        columns,
        dictionary: e.dictionary,
        aux: e.aux,
    };
    let mut out = MAGIC.to_vec();
    out.extend(postcard::to_allocvec(&payload)?);
    if out.len() > MAX {
        return Err(FormatError::GraphFieldLimitExceeded);
    }
    Ok(out)
}
