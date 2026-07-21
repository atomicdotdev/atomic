# Learned Representations of the Patch Monoid: A Position Paper on Neural Graph Embeddings for the Semantic Change Graph

**Lee Faus, July 2026**

---

## Abstract

We argue that the *semantic change graph* — a content-addressed DAG where changes are composable operations with typed edges, CRDT hierarchy, and view-filter semantics — admits a natural class of neural representations that no snapshot-based VCS can support. The key insight is that the patch monoid $(M, \circ)$ underlying such systems has structure that a learned homomorphism $\rho: M \to \text{GL}(V)$ can exploit: composition becomes matrix multiplication, independence becomes commutativity, and **conflict detection reduces to measuring non-commutativity** via the Lie bracket $\|[\rho_A, \rho_B]\|$.

We formalize five invariants that such a representation must satisfy, propose a four-layer encoder architecture that mirrors the data model of Atomic (a patch-theory VCS built in Rust), and show that the resulting system — a *Patch Language Model* (PLM) — is feasible at repository scale (10⁴–10⁶ vertices) on commodity hardware. We further argue that content-addressed hashing provides a free, exact join key for cross-project federation, enabling workspace-scale models without entity resolution.

This is a position paper. No experimental results are reported. The contribution is the mathematical framework linking patch algebra to representation theory, the architecture that enforces algebraic invariants by construction, and the argument that this specific combination of properties is unique to the semantic change graph and cannot be replicated atop Git.

---

## 1. Introduction: Why the Semantic Change Graph Changes Everything

Version control systems store code history. The dominant paradigm — Git — represents history as a DAG of *snapshots*: each commit is a complete tree of file contents, and diffs are computed post hoc by comparing trees. Machine learning over Git histories therefore operates on *derived* structure: commit messages, file-level diffs, co-change frequencies, AST deltas. The underlying data model offers no algebraic guarantees.

Atomic takes a fundamentally different approach. Its core data structure is the **semantic change graph**: a content-addressed DAG where changes are first-class composable operations with typed edges, a CRDT semantic hierarchy (Trunk → Branch → Leaf), view-filter semantics, and well-defined dependency relations and commutativity conditions. A file is not a blob; it is a DAG of vertices (content hunks) and edges (ordering relations with typed flags). A "merge" is not a three-way text diff; it is the insertion of change references into a view's filter set, with conflict arising when two changes' graph operations fail to commute.

This structure — the semantic change graph — is precisely the kind of inductive bias that neural networks can exploit, but only if the representation is designed to preserve it. A naive graph neural network over the repository graph would learn *something*, but would not be constrained to respect the patch algebra. We propose a representation that is.

### 1.1 Thesis

**The semantic change graph admits a learned representation — a monoid homomorphism from the patch monoid into a matrix group — that (a) preserves composition, (b) detects conflict via non-commutativity, (c) encodes the CRDT semantic hierarchy in hyperbolic space, and (d) enables four practically useful downstream tasks (conflict prediction, dependency link prediction, content reconstruction, next-change forecasting) via a single self-supervised training objective. This representation is unique to the semantic change graph: no snapshot-based system provides the structure to define or enforce it.**

### 1.2 Why This Cannot Be Done on Git

Three properties of the semantic change graph are essential and absent from Git:

1. **Typed, relational edges.** Atomic's graph has edges carrying `EdgeFlags` (BLOCK, PSEUDO, FOLDER, PARENT, DELETED — 5 base flags, up to 32 combinations). Each edge records which change introduced it (`introduced_by`). Git has no edge types; its DAG is commits pointing to parent commits.

2. **Explicit commutativity.** In Atomic, two changes are independent ($p_A \perp p_B$) if and only if their graph operations touch disjoint vertices and edges. This is a *decidable, structural property* of the changes themselves. In Git, whether two commits "conflict" is an emergent property of the three-way merge algorithm applied to text — it depends on the merge strategy, not the commits' algebraic relationship.

3. **View filters as change-set masks.** Atomic's views are not branches (pointers to commits); they are ordered sets of change references filtering a single canonical graph. The VIEW_CHANGES table *is* the attention mask for neural inference — the architecture constraint (invariant I4) has a direct physical realization in the storage layer.

---

## 2. The Representation: Patch Monoid → Matrix Group

### 2.1 Formal Setup

Let $(M, \circ)$ be the patch monoid: the set of all changes under composition. We seek a representation

$$\rho: M \to \text{GL}(\mathbb{R}^d)$$

such that the homomorphism property holds approximately:

$$\|\rho(p_1 \circ p_2) - \rho(p_1) \cdot \rho(p_2)\| < \epsilon$$

This is enforced during training via a **composition loss**: sample change pairs $(p_1, p_2)$ where $p_1 \circ p_2$ is recorded, compute $\rho$ for all three, and penalize deviation from multiplicativity.

### 2.2 Five Invariants

The representation must satisfy five invariants, each enforced by a specific architectural or training mechanism:

| ID | Invariant | Statement | Mechanism |
|----|-----------|-----------|-----------|
| **I1** | Composition | $\rho(p_1 \circ p_2) \approx \rho(p_1) \cdot \rho(p_2)$ | Composition loss |
| **I2** | Independence = commutativity | $p_A \perp p_B \Rightarrow \rho(p_A)\rho(p_B) = \rho(p_B)\rho(p_A)$ | DeepSets pooling (permutation-invariant by construction) |
| **I3** | Conflict = non-commutativity | $\|[\rho_A, \rho_B]\| > \tau \Leftrightarrow \text{conflict}(p_A, p_B)$ | Conflict head with bracket loss |
| **I4** | View filter = attention mask | VIEW_CHANGES membership ≡ forward-pass visibility | Architectural constraint |
| **I5** | Hierarchy = hyperbolic distance | Trunk–Branch–Leaf ordering preserved by $d_{\mathbb{H}}$ | Curvature objective |

The bracket $[\rho_A, \rho_B] = \rho_A\rho_B - \rho_B\rho_A$ is the Lie bracket (commutator). Its norm is zero for independent changes and non-zero for conflicting ones. This is the core mathematical contribution: **conflict detection is a measurement of non-commutativity in representation space.**

In practice, the full bracket requires $d \times d$ matrix representations per change ($d^2$ parameters). The default is a **bilinear approximation**:

$$\text{conflict\_score}(p_A, p_B) = \sigma\!\left(z_A^\top W_{\text{bracket}}\, z_B \;-\; z_B^\top W_{\text{bracket}}\, z_A\right)$$

where $z_A, z_B \in \mathbb{R}^d$ are change embeddings and $W_{\text{bracket}} \in \mathbb{R}^{d \times d}$ is learned. This captures the antisymmetric structure of the bracket without the $O(d^2)$-per-change storage cost.

### 2.3 Why Hyperbolic Geometry for the CRDT Layer

Atomic's semantic layer is a three-level hierarchy: **Trunk** (file) → **Branch** (line) → **Leaf** (token). Each level has typed IDs (TrunkId, BranchId, LeafId — all 12 bytes: 8-byte NodeId + 4-byte index). This is a tree with branching factor proportional to lines-per-file and tokens-per-line.

Poincaré embeddings [Nickel & Kiela, NeurIPS 2017] demonstrated that **5-dimensional hyperbolic embeddings match 200-dimensional Euclidean embeddings** for hierarchy encoding on WordNet. HGCN [Chami et al., NeurIPS 2019] showed up to **63.1% error reduction** in ROC AUC for link prediction on tree-like graphs (Gromov hyperbolicity δ ≈ 0) using trainable per-layer curvature. Atomic's CRDT tree has δ = 0 — precisely the regime where hyperbolic geometry provides maximum advantage.

The curvature objective enforces:

$$d_{\mathbb{H}}(\text{trunk}, \text{branch}_i) < d_{\mathbb{H}}(\text{trunk}, \text{leaf}_{ij})$$
$$d_{\mathbb{H}}(\text{branch}_i, \text{branch}_j) > d_{\mathbb{H}}(\text{trunk}, \text{branch}_i) \quad \text{(siblings more distant than parent–child)}$$

Each hierarchy level gets its own learned curvature $c_\ell$ (scalar), following HGCN's approach.

---

## 3. Architecture: Four Layers Mirroring the Data Model

The encoder stack has four layers, each corresponding to a level of Atomic's data model:

```
L1  Vertex Encoder      GRAPH + INODE_GRAPH       CompGCN (relational convolution)
L2  Semantic Encoder     CRDT tables (T/B/L)       Poincaré pooling (hyperbolic)
L3  Change Encoder       touched vertices + ops     DeepSets (set pooling)
L4  View Encoder         VIEW_CHANGES + DEPS       DAG attention (causal)
```

This is not an accident. The architecture is the data model's mirror image — each table family maps to exactly one encoder layer, and the encoder invariants map to the algebraic properties of the corresponding data structures.

### 3.1 L1: Relational Graph Convolution (CompGCN)

The repository graph has 32 typed edge relations (combinations of BLOCK, PSEUDO, FOLDER, PARENT, DELETED flags) and each edge records its introducing change. CompGCN [Vashishth et al., ICLR 2020] jointly embeds nodes and relations via composition:

$$h_v^{(\ell+1)} = f\!\left(\sum_{(u,r)\in\mathcal{N}(v)} W_\lambda^{(\ell)} \;\phi(h_u^{(\ell)}, h_r^{(\ell)})\right)$$

where $\phi$ is a composition function (subtraction, multiplication, or circular correlation) and $\lambda \in \{O, I, S\}$ indexes original, inverse, and self-loop weight matrices. This achieves MRR=0.355 on FB15k-237 vs R-GCN's 0.248, with parameter complexity $O(Kd^2 + Bd + B|R|)$ vs R-GCN's $O(BKd^2 + BK|R|)$ — though for Atomic's small $|R| \leq 32$, R-GCN's parameter explosion is manageable and remains a viable baseline.

Relational Graph Attention Networks [Busbridge et al., 2019] were considered and rejected: they empirically underperform spectral methods on transductive tasks with fixed schemas.

**Vertex input features:** content length, content hash (learnable embedding with hash_embed cold-start), is-empty flag (inode markers), change ID embedding, and temporal position (normalized sequence index within the view). The temporal feature is motivated by the finding that static graph structure alone does not improve over sequence models for JIT defect prediction [Milewicz et al., JITGNN, JSS 2024], while temporal graph representations significantly outperform static ones for change propagation [Zaraket et al., IST 2024].

**Configuration:** $d = 64$, 2 layers, $B = 5$ basis vectors, ReLU activation. ~50K parameters.

### 3.2 L2: Poincaré Hierarchy Pooling

L1 vertex embeddings are projected into the Poincaré ball $\mathbb{B}^{16}$ via the exponential map. Leaf embeddings within a branch are aggregated via the Möbius midpoint; branch embeddings are pooled to trunk embeddings likewise. Each level has a learnable curvature scalar $c_\ell$.

$d = 16$ in hyperbolic space. ~5K parameters.

### 3.3 L3: DeepSets Change Encoder

A change touches a *set* of vertices (no canonical ordering). DeepSets [Zaheer et al., NeurIPS 2017] provides a universal approximator for permutation-invariant functions:

$$z_c = \text{MLP}_\phi\!\left(\sum_{v \in \text{touched}(c)} \text{MLP}_\psi(z_v) \;+\; \text{OpEmbed}(c)\right)$$

where OpEmbed$(c)$ sums learned embeddings for each operation in the change (TrunkOp × 4, BranchOp × 3, LeafOp × 4, TokenKind × $k$). This satisfies I2 by construction: permuting the inputs does not change the output.

Set Transformer [Lee et al., ICML 2019] is a richer alternative (attention over sets with inducing points) but adds cost without clear benefit at repository scale. Retained as an ablation option.

$d = 64$. ~20K parameters.

### 3.4 L4: DAG-Aware View Encoder

A view is an ordered set of change references (VIEW_CHANGES) with dependency structure (DEPS). The base encoder is DeepSets over change embeddings; the upgrade applies causal attention after topological sorting by the DEPS partial order — each change attends only to its dependencies.

This naturally implements I4: the VIEW_CHANGES filter *is* the attention mask. Changes outside the view are invisible to the encoder, matching the semantic guarantee of Atomic's view model.

$d = 64$. ~15K parameters.

### 3.5 Total: ~90K Parameters

This is intentionally tiny. A per-repo model should train in minutes on CPU, infer in milliseconds, and cold-start gracefully. The existing `hash_embed` fallback in Atomic's AI module provides the precedent for local-only, no-API-key operation.

---

## 4. Four Prediction Heads, One Self-Supervised Objective

All four heads are trained jointly via self-supervised objectives derived from the repository's own history. No external labels are needed.

### 4.1 Conflict Prediction (Bracket Head)

**Signal:** When two changes produce PSEUDO edges or conflict markers during apply, they are labeled as conflicting. Negative samples: change pairs coexisting in the same view without conflict.

**Evaluation:** F1, precision, recall. The literature baseline is F1 = 0.57–0.68 for the conflict minority class on 267K merge scenarios from 744 GitHub repos [Owhadi-Kareshk et al., ESEM 2019]. Microsoft's ConE system — a non-ML heuristic using file overlap and rarity — achieves >70% developer approval in production on 234 repos [Nitu et al., FSE 2021]. Any learned system must demonstrably outperform these baselines.

**Design note:** Conflict prediction is asymmetric. Safe merges are easy to predict (F1 ~0.96); actual conflicts are the minority class and are hard. The practical application is **filtering likely-safe scenarios**, not precisely predicting conflict.

### 4.2 Dependency Link Prediction

**Signal:** Positive edges from DEPS; negatives from random non-dependent pairs (filtered setting).

**Evaluation:** Filtered MRR, Hits@{1,3,10} with time-split (train on past, test on future). Use strat-MRR [Mohamed et al., AISTATS 2020] to avoid bias toward popular files. Ali et al. (PyKEEN, 2021) found many published KGE results non-reproducible — the evaluation harness must log exact hyperparameters and seeds.

### 4.3 Content Reconstruction (Masked Vertex)

**Signal:** Mask 15% of vertex features; predict from neighborhood context (BERT-style, adapted to graphs). The striff-gnn project uses this exact objective on software graphs.

**Evaluation:** Reconstruction MSE.

### 4.4 Next-Change Forecasting

**Signal:** Given a view's change sequence $[c_1, \ldots, c_t]$, predict $c_{t+1}$.

This is the "language modeling" objective that makes the system a Patch Language Model. The vocabulary is graph operations; the grammar is the patch algebra; a sentence is a view's change history.

**Evaluation:** MRR, Hits@k. Baseline: frequency and recency heuristics. For context, CoEdPilot [ISSTA 2024] achieves 70.8–85.3% edit location accuracy on 180K commits using multi-component neural transformers — the next-change head is a structural analogue at the change level.

---

## 5. The Patch Language Model

### 5.1 Per-Project PLM

A single repository's history (10³–10⁵ changes) trains a small foundation model of that project's evolution:

| Language Model Concept | Patch Language Model |
|------------------------|---------------------|
| Vocabulary | GraphOps + TrunkOp/BranchOp/LeafOp + TokenKind + 32 EdgeFlags |
| Grammar | Patch algebra (composition, dependency closure, context positions) |
| Sentence | A view's change sequence (VIEW_CHANGES + DEPS partial order) |
| Next-token objective | $P(c_{t+1} \mid z_{\text{view}})$ |
| Masked-token objective | Masked vertex/edge prediction over GRAPH |

The PLM is not a prose LLM. It does not generate text or explain code. It computes embeddings, conflict scores, and next-change distributions. It *fronts* an LLM: the PLM provides structural retrieval and scoring; the LLM verbalizes. Math does the reasoning; the LLM does the talking.

### 5.2 Workspace Federation

Three mathematical properties of the semantic change graph make cross-project federation possible — two are unique to content-addressed patch systems:

**Shared structural grammar.** The relation alphabet (EdgeFlags combinations, op types, dependency rules) is project-independent. The L1/L2 encoder layers transfer across projects; only content and change embeddings are project-specific. This parallels multilingual language models: shared syntactic layers, per-language lexicons.

**Content-addressed join keys.** Blake3 hashes are globally content-defined. Two repos recording identical hunks produce identical hashes. A multi-repo corpus requires *zero entity resolution* — concatenation is deduplication. No other VCS-ML pipeline gets this property for free; Git SHAs are content-addressed but Git's data model lacks the semantic change graph's typed-edge structure to make the hashes useful as embedding keys.

**Graph of graphs.** Real cross-repo dependencies exist (atomic-cli depends on atomic-core). The workspace model uses cross-repo attention over a super-graph whose inter-repo edges come from dependency manifests. Blast radius crosses repo boundaries — a capability no tool on the market provides.

### 5.3 The Recursion (Speculative)

Model merging is itself patch theory. Fine-tune the workspace model per project: $\Delta\theta$ is a parameter-space patch. Two divergent fine-tunes are two views; merging them (model soup / task arithmetic) is a weight-space merge whose conflict measure is the same bracket norm $\|[\rho_A, \rho_B]\|$. Atomic can version the weights of the model that versions Atomic's changes — the change DAG of the PLM's own training runs, stored in Atomic, content-addressed, diffable. This is noted as a speculative observation, not a near-term deliverable.

---

## 6. Feasibility and Risks

### 6.1 Scale

At 10⁴–10⁶ vertices per repository, this is tiny by GNN standards. All architectures discussed (R-GCN, CompGCN, HGCN) operate comfortably at this scale on single GPU or CPU. The HGCN reference implementation trains with $d = 16$, 2 layers, 5000 epochs on datasets of comparable size.

### 6.2 Inference in Pure Rust

Candle (Hugging Face's pure-Rust ML framework) supports the operations needed for GNN message passing: `index_select`, `index_add`, `scatter_add`, `matmul`. A complete GCN training implementation in candle exists [wdoppenberg, GitHub gist]. Candle loads safetensors natively via zero-copy memory mapping and achieves near-parity with PyTorch for matmul-dominated workloads (35 tok/s vs 34 tok/s, Llama-3.2-1B, M4 Max). Candle has no C dependencies, matching Atomic's redb choice philosophy. If candle CPU performance proves insufficient, the `ort` crate (ONNX Runtime for Rust, 13M+ downloads) provides a fallback path.

### 6.3 Risk Matrix

| Risk | Severity | Mitigation |
|------|----------|------------|
| Per-repo distribution shift | Medium | Per-repo self-supervision + hash_embed cold start (precedented) |
| Small training data (10³–10⁵ changes) | Medium | Intentionally small model (90K params); corpus mode adds generalization |
| Hyperbolic numerical instability | Medium | Hyperboloid model for computation, Poincaré for display [HGCN]; norm clamping |
| **Learned indexes on correctness path** | **High** | **Strict policy: embeddings are advisory only.** Merkle stays the identity layer. Conflict scores are warnings, not vetoes. No learned component gates record/apply. |
| Graph structure adds no value (JITGNN negative result) | Medium | Atomic's graph is richer than ASTs (typed edges, CRDT hierarchy, views). Combine structural + semantic features [Athena pattern]. Ablate to verify. |
| Evaluation reproducibility | Medium | Log exact hyperparameters, seeds, timestamps. Use strat-MRR for unbiased evaluation. |

The highest-severity risk is deliberate: **learned indexes must never enter the correctness path.** Merkle hashes provide identity, integrity, and convergence. Embeddings provide suggestions, rankings, and early warnings. The separation is a hard architectural constraint, not a policy preference.

---

## 7. Related Work

### 7.1 GNN on Version Control

No prior work applies relational GNNs to semantic change graphs. Existing approaches operate on Git:

- **CC2Vec / commit2vec**: Embed commit diffs as fixed-size vectors for defect prediction. Operate on text diffs, not typed graph operations.
- **DeepJIT** [Hoang et al., MSR 2019]: End-to-end CNN on commit messages + code diffs. No graph structure.
- **JITGNN** [Milewicz et al., JSS 2024]: GNN on ASTs of changed code. Notable negative result: graph structure did *not* improve over sequence models. We argue Atomic's richer graph (typed edges, CRDT hierarchy, view structure) provides inductive bias that ASTs lack.
- **striff-gnn** [GitHub]: Heterogeneous Graph Transformer on software architecture graphs with masked edge reconstruction. Closest in spirit to our content reconstruction head.

### 7.2 Conflict Prediction

- **Owhadi-Kareshk et al.** [ESEM 2019]: ML on Git metadata features. F1 0.57–0.68 on conflict minority class across 267K merge scenarios.
- **ConE** [Microsoft, FSE 2021]: Non-ML heuristic (file overlap + rarity). Deployed on 234 repos, >70% developer approval.
- **Al-Refai et al.** [JSMR 2025]: Stacking ensembles. Social + technical features both matter.
- **Merge-Bench** [Schesch et al., 2025]: 7,938 hunks from 1,439 repos, 11 languages. Best available benchmark for conflict resolution.

All operate on Git. None exploit the semantic change graph's algebraic commutativity.

### 7.3 Impact Analysis

- **Athena** [FSE 2024]: CodeBERT + program dependence graphs for method-level impact analysis. mRR 60.32%, HIT@10 81.48%. Demonstrates that combining structural and semantic information outperforms either alone — a finding we build on.
- **Rex** [Microsoft, NSDI 2020]: Correlated change analysis for production services.
- **CoEdPilot** [ISSTA 2024]: Multi-component neural system for ripple-effect estimation. Edit location accuracy 70.8–85.3%.
- **Temporal graph approaches** [Zaraket et al., IST 2024]: Temporal Graph Networks outperform static graphs for change propagation.

### 7.4 Hyperbolic Embeddings

- **Poincaré Embeddings** [Nickel & Kiela, NeurIPS 2017]: 5D hyperbolic ≈ 200D Euclidean for hierarchy encoding.
- **HGCN** [Chami et al., NeurIPS 2019]: First inductive hyperbolic GCN. Trainable per-layer curvature. Up to 63.1% error reduction on tree-like graphs.
- **MuRP** [Balažević et al., NeurIPS 2019]: Per-relation transformations in Poincaré ball for multi-relational KGs.

---

## 8. Conclusion

We have argued that the semantic change graph possesses mathematical structure — the patch monoid, typed relational edges, explicit commutativity, CRDT hierarchies, and view-filter semantics — that admits a class of learned representations unavailable to snapshot-based systems. The representation is a monoid homomorphism enforced by five invariants, realized through a four-layer encoder mirroring the data model, and trained via self-supervised objectives requiring no external labels.

The practical payoff is a *Patch Language Model*: a small (~90K parameter), local, per-project foundation model that provides conflict early-warning, dependency prediction, blast-radius estimation, and next-change forecasting — all advisory, never on the correctness path. Content-addressed hashing provides free cross-project join keys for workspace federation.

The unique contribution is not any individual component (GNNs, hyperbolic embeddings, DeepSets, and conflict prediction all exist independently) but their *composition under the constraints of patch algebra*. The invariants are not regularizers added for aesthetic reasons; they are structural properties of the data model that the representation must preserve to be useful. This is the sense in which the architecture is the data model's mirror image.

No experiments are reported. The position is falsifiable: if the bracket norm does not separate conflicting from independent change pairs, the central thesis fails. We consider this the first experiment worth running.

---

## References

1. Schlichtkrull, M. et al. "Modeling Relational Data with Graph Convolutional Networks." ESWC 2018. https://arxiv.org/abs/1703.06103
2. Vashishth, S. et al. "Composition-based Multi-Relational Graph Convolutional Networks." ICLR 2020. https://arxiv.org/abs/1911.03082
3. Busbridge, D. et al. "Relational Graph Attention Networks." 2019. https://arxiv.org/abs/1904.05811
4. Nickel, M. & Kiela, D. "Poincaré Embeddings for Learning Hierarchical Representations." NeurIPS 2017. https://arxiv.org/abs/1705.08039
5. Chami, I. et al. "Hyperbolic Graph Convolutional Neural Networks." NeurIPS 2019. https://arxiv.org/abs/1910.12933
6. Balažević, I. et al. "Multi-relational Poincaré Graph Embeddings." NeurIPS 2019.
7. Zaheer, M. et al. "Deep Sets." NeurIPS 2017. https://arxiv.org/abs/1703.06114
8. Lee, J. et al. "Set Transformer: A Framework for Attention-based Permutation-Invariant Input." ICML 2019. https://arxiv.org/abs/1810.00825
9. Owhadi-Kareshk, M. et al. "Predicting Merge Conflicts in Collaborative Software Development." ESEM 2019. https://arxiv.org/abs/1907.06274
10. Nitu, P. et al. "ConE: A Concurrent Edit Detection Tool for Large Scale Software Development." FSE 2021. https://ar5iv.labs.arxiv.org/html/2101.06542
11. Al-Refai, M. et al. "Merge Conflict Prediction Using Feature Selection and Stacking Heterogeneous Ensembles." JSMR 2025. https://doi.org/10.1002/smr.70047
12. Milewicz, R. et al. "JITGNN: A Deep Graph Neural Network Framework for Just-In-Time Bug Prediction." JSS 2024. https://doi.org/10.1016/j.jss.2024.111984
13. Yan, Y. et al. "Athena: Enhancing Code Understanding for Impact Analysis by Combining Transformers and PDGs." FSE 2024. https://doi.org/10.1145/3643770
14. Zaraket, F. et al. "Change propagation using Temporal Graphs and GNNs." IST 2024.
15. Mehta, S. et al. "Rex: Preventing Bugs and Misconfiguration Failures Using Correlated Change Analysis." NSDI 2020. https://www.usenix.org/system/files/nsdi20-paper-mehta.pdf
16. Hoang, T. et al. "DeepJIT: An End-to-End Deep Learning Framework for Just-In-Time Defect Prediction." MSR 2019.
17. CoEdPilot. ISSTA 2024. https://arxiv.org/html/2408.01733v1
18. Schesch, B. et al. "Merge-Bench." 2025. https://github.com/benedikt-schesch/Merge-Bench
19. Bordes, A. et al. "Translating Embeddings for Modeling Multi-relational Data." NeurIPS 2013.
20. Berrendorf, M. et al. "A Unified Framework for Rank-based Evaluation Metrics for Link Prediction in Knowledge Graphs." 2022. https://arxiv.org/abs/2203.07544
21. Mohamed, E. et al. "Popularity Agnostic Evaluation of Knowledge Graph Embeddings." AISTATS 2020.
22. Ali, M. et al. "Bringing Light Into the Dark: A Large-scale Evaluation of Knowledge Graph Embedding Models Under a Unified Framework." (PyKEEN). 2021.
23. Vale, G. et al. "Predicting merge conflicts considering social and technical assets." EMSE 2023. https://link.springer.com/article/10.1007/s10664-023-10395-8
24. Candle ML Framework. Hugging Face. https://github.com/huggingface/candle
25. Safetensors Format. Hugging Face. https://github.com/huggingface/safetensors
26. wdoppenberg. "GCN ENZYMES in Candle." GitHub Gist. https://gist.github.com/wdoppenberg/a22847d84ab55002e002d7828eb03934
27. Candle GCNConv Discussion #2151. https://github.com/huggingface/candle/discussions/2151
28. ort — ONNX Runtime for Rust. https://github.com/pykeio/ort
29. striff-gnn. https://github.com/hadi-technology/striff-gnn
30. Dias, A. et al. "On the Prediction of Software Merge Conflicts: A Systematic Literature Review." ACM 2023. https://dl.acm.org/doi/fullHtml/10.1145/3592813.3592931

---

## Appendix A: Proposed Implementation Outline

The position paper's architecture maps to two codebases:

**Rust crate (`atomic-neural`):** Export pipeline (read-only pristine → Parquet), candle-based inference (CompGCN + Poincaré + DeepSets + DAG attention + 4 heads), CHANGE_EMBEDDINGS table in pristine, CLI integration (`atomic neural export`, `atomic neural embed`, conflict score in `record` output, `atomic change --similar`).

**Python package (`neural/`):** PyTorch Geometric training with geoopt for Poincaré operations. Self-supervised multi-objective training loop. Weight export via safetensors. Evaluation harness with filtered MRR, Hits@k, F1, strat-MRR, and seed logging.

**Export schema:** Corpus-ready from day one. Per-repo directories with `manifest.json` + Parquet tables (nodes, edges, changes, views, CRDT trunks/branches/leaves, deps). `repo_id` + content-addressed `hash` columns provide globally unique join keys across repos.

**Schema addition:** `CHANGE_EMBEDDINGS: TableDefinition<u64, &[u8]>` in redb, keyed by change NodeId, following the existing EMBEDDINGS table pattern.
